import AppKit
import SwiftUI

/// Every file's diff in one lazy scroll, like GitHub, with a sticky header per file.
/// Click a line to select it, shift-click to extend; Esc clears.
struct DiffList: View {
    let review: PRReview
    let files: [DiffFileModel]
    @Binding var expanded: [String: Bool]
    @Binding var editor: DiffEditorTarget?
    let scrollRequest: DiffScrollRequest?
    let setViewed: (String, Bool) -> Void
    /// Where a shift-click extends from: the last plain click, if it's still an end of the selection.
    @State private var anchor: (path: String, row: Int)?
    /// Files whose header notes (outdated, latest-push and pending comments) are expanded.
    @State private var shownNotes: Set<String> = []
    @State private var escapeMonitor: Any?

    var body: some View {
        let opened = review.opened
        let pendingReview = opened?.detail.pendingReview
        let annotations = DiffAnnotations(threads: opened?.detail.threads ?? [], drafts: review.drafts,
                                          pending: pendingReview?.comments ?? [],
                                          threadsInline: opened?.detail.headOid == opened?.checkout.headOid,
                                          pendingInline: pendingReview?.commitOid == opened?.checkout.headOid)
        let viewed = review.viewedPaths
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0, pinnedViews: .sectionHeaders) {
                    ForEach(files) { file in
                        let isExpanded = expanded[file.id] ?? !(viewed.contains(file.id) || file.isLarge)
                        let notes = annotations.notes[file.id] ?? DiffFileNotes()
                        Section {
                            if shownNotes.contains(file.id) && !notes.isEmpty {
                                DiffFileNotesView(notes: notes, review: review)
                            }
                            if isExpanded {
                                content(of: file, annotations: annotations)
                            } else if file.isLarge && !viewed.contains(file.id) {
                                DiffNotice(text: "\(file.lineCount.formatted()) changed lines are hidden to keep scrolling fast.",
                                           buttonTitle: "Show Diff") { expanded[file.id] = true }
                            }
                        } header: {
                            DiffFileHeader(file: file, isExpanded: isExpanded, isViewed: viewed.contains(file.id),
                                           canMarkViewed: !review.hasNewerCommits,
                                           notes: notes.isEmpty ? nil : notes.summary,
                                           isShowingNotes: shownNotes.contains(file.id)) {
                                expanded[file.id] = !isExpanded
                                // Collapsing from deep inside a file would otherwise leave you in the next one.
                                if isExpanded { proxy.scrollTo(file.id, anchor: .top) }
                            } toggleNotes: {
                                shownNotes.formSymmetricDifference([file.id])
                            } setViewed: { setViewed(file.id, $0) }
                            .id(file.id)
                        }
                    }
                }
            }
            .onChange(of: scrollRequest) { _, request in
                if let request { proxy.scrollTo(request.path, anchor: .top) }
            }
        }
        // A monitor rather than onKeyPress, which needs the diff focused and clicks don't focus it.
        // Esc stays with text fields and editors (chat, composer) and with sheets.
        .onAppear {
            escapeMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [review] event in
                guard event.keyCode == 53, review.selection != nil, event.window?.attachedSheet == nil,
                      !(event.window?.firstResponder is NSText)
                else { return event }
                review.selection = nil
                return nil
            }
        }
        .onDisappear {
            if let escapeMonitor { NSEvent.removeMonitor(escapeMonitor) }
        }
    }

    @ViewBuilder
    private func content(of file: DiffFileModel, annotations: DiffAnnotations) -> some View {
        if file.file.isBinary {
            DiffNotice(text: "Binary file not shown.")
        } else if file.rows.isEmpty {
            DiffNotice(text: file.file.status == .renamed ? "File renamed without changes." : "No changes to show.")
        } else {
            let selected = file.rowRange(of: review.selection)
            ForEach(file.rows) { row in
                // One view per row, so the lazy stack can count rows without building them.
                DiffRowView(row: row, numberWidth: file.numberWidth, isSelected: selected?.contains(row.position) ?? false,
                            // Hidden while that selection's own composer is open right below it.
                            actionSelection: selected?.upperBound == row.position && editor?.newComment != review.selection
                                ? review.selection : nil,
                            // Varies only on the bar's row, so opening an editor doesn't redraw every row.
                            canComment: editor == nil || selected?.upperBound != row.position,
                            comments: comments(at: row, in: file, annotations: annotations),
                            review: review, editor: $editor) { select(row.position, in: file) }
                    .equatable()
            }
        }
    }

    private func comments(at row: DiffRow, in file: DiffFileModel, annotations: DiffAnnotations) -> DiffRowComments? {
        guard case .line(let line) = row.kind else { return nil }
        let anchors = DiffAnnotations.anchors(of: line, in: file.id)
        let threads = anchors.flatMap { annotations.threads[$0] ?? [] }
        let drafts = anchors.flatMap { annotations.drafts[$0] ?? [] }
        let pending = anchors.flatMap { annotations.pending[$0] ?? [] }
        let composing = editor?.newComment.flatMap {
            DiffAnchor(path: $0.path, side: $0.side, line: $0.line) == anchors[0] ? $0 : nil
        }
        guard !threads.isEmpty || !drafts.isEmpty || !pending.isEmpty || composing != nil else { return nil }
        return DiffRowComments(threads: threads, pending: pending, drafts: drafts, composing: composing, editor: editor)
    }

    private func select(_ row: Int, in file: DiffFileModel) {
        if NSEvent.modifierFlags.contains(.shift), let current = file.rowRange(of: review.selection) {
            let from = anchor.flatMap { $0.path == file.id && ($0.row == current.lowerBound || $0.row == current.upperBound) ? $0.row : nil }
                ?? current.lowerBound
            review.selection = file.selection(from: min(from, row), to: max(from, row))
        } else {
            anchor = (file.id, row)
            review.selection = file.selection(from: row, to: row)
        }
    }
}

/// What shows under a line. `editor` is included so rows redraw when it opens or closes.
struct DiffRowComments: Equatable {
    let threads: [ReviewThread]
    let pending: [PendingComment]
    let drafts: [DraftComment]
    let composing: Selection?
    let editor: DiffEditorTarget?
}

/// One row of a file's diff: a hunk header, or a line with its comments underneath.
/// Equatable without its closures, so selection changes only redraw the rows they touch.
struct DiffRowView: View, Equatable {
    let row: DiffRow
    let numberWidth: CGFloat
    let isSelected: Bool
    /// Set on the selection's last row, which carries the action bar.
    let actionSelection: Selection?
    /// False while another editor is open, so its text isn't lost.
    let canComment: Bool
    let comments: DiffRowComments?
    let review: PRReview
    @Binding var editor: DiffEditorTarget?
    let select: () -> Void

    var body: some View {
        switch row.kind {
        case .hunk(let header):
            DiffHunkRow(header: header, numberWidth: numberWidth)
        case .line(let line):
            VStack(alignment: .leading, spacing: 0) {
                DiffLineRow(line: line, numberWidth: numberWidth, isSelected: isSelected)
                    .onTapGesture(perform: select)
                    // One element per line, selectable with VoiceOver like a click.
                    .accessibilityElement(children: .combine)
                    .accessibilityAddTraits(isSelected ? [.isButton, .isSelected] : .isButton)
                    .accessibilityAction { select() }
                    .overlay(alignment: .trailing) {
                        if let actionSelection {
                            DiffSelectionBar(review: review, canComment: canComment) { editor = .newComment(actionSelection) }
                        }
                    }
                if let comments {
                    DiffLineComments(review: review, threads: comments.threads, pending: comments.pending,
                                     drafts: comments.drafts,
                                     composing: comments.composing, editor: $editor)
                }
            }
        }
    }

    static func == (a: Self, b: Self) -> Bool {
        a.row == b.row && a.numberWidth == b.numberWidth && a.isSelected == b.isSelected
            && a.actionSelection == b.actionSelection && a.canComment == b.canComment && a.comments == b.comments && a.review === b.review
    }
}

/// One diff line: old number, new number, then the text, tinted by kind.
struct DiffLineRow: View {
    let line: DiffLine
    let numberWidth: CGFloat
    let isSelected: Bool
    @Environment(\.palette) private var palette

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 0) {
            HStack(spacing: 0) {
                Text(line.oldLine.map(String.init) ?? "").frame(width: numberWidth, alignment: .trailing)
                Text(line.newLine.map(String.init) ?? "").frame(width: numberWidth, alignment: .trailing)
            }
            .foregroundStyle(.secondary)
            .padding(.trailing, 4)
            .background(isSelected ? palette.selection.opacity(DiffStyle.selectedOpacity) : tint.opacity(DiffStyle.gutterOpacity))
            Text(verbatim: marker + line.text)
                .padding(.leading, 4)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(DiffStyle.font)
        .padding(.vertical, 1)
        .background(isSelected ? palette.selection.opacity(DiffStyle.selectedOpacity) : tint.opacity(DiffStyle.rowOpacity))
        .contentShape(Rectangle())
    }

    private var tint: Color {
        switch line.kind {
        case .added: palette.added
        case .removed: palette.removed
        case .context: .clear
        }
    }

    private var marker: String {
        switch line.kind {
        case .added: "+ "
        case .removed: "- "
        case .context: "  "
        }
    }
}

/// "@@ -1,4 +1,5 @@ fn main()"; hunk headers can't take comments.
struct DiffHunkRow: View {
    let header: String
    let numberWidth: CGFloat

    var body: some View {
        Text(verbatim: header)
            .font(DiffStyle.font)
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .padding(.leading, numberWidth * 2 + 8)
            .padding(.vertical, 4)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(DiffStyle.hunk)
    }
}

/// The sticky bar above each file: collapse chevron, path, counts, comments not on a line, and Viewed.
struct DiffFileHeader: View {
    let file: DiffFileModel
    let isExpanded: Bool
    let isViewed: Bool
    let canMarkViewed: Bool
    /// Summary of the comments listed from the header instead of under a line, if any.
    let notes: String?
    let isShowingNotes: Bool
    let toggleExpanded: () -> Void
    let toggleNotes: () -> Void
    let setViewed: (Bool) -> Void

    var body: some View {
        HStack(spacing: 8) {
            Button(action: toggleExpanded) {
                Image(systemName: "chevron.right")
                    .rotationEffect(.degrees(isExpanded ? 90 : 0))
                    .frame(width: 14, height: 14)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(isExpanded ? "Collapse this file" : "Expand this file")
            DiffStatusMark(status: file.file.status)
            DiffPathText(file: file.file, renameArrow: true)
                .font(.callout)
                .textSelection(.enabled)
            DiffLineCounts(additions: file.additions, deletions: file.deletions)
            Spacer(minLength: 8)
            if let notes {
                Button(action: toggleNotes) {
                    Label(notes, systemImage: "clock.arrow.circlepath")
                }
                .buttonStyle(.link)
                .font(.caption)
                .help(isShowingNotes ? "Hide comments that aren't on a line of this diff"
                                     : "Show comments that aren't on a line of this diff")
            }
            Toggle("Viewed", isOn: Binding(get: { isViewed }, set: setViewed))
                .toggleStyle(.checkbox)
                .font(.callout)
                .disabled(!canMarkViewed)
                .help(canMarkViewed ? "Mark as viewed on GitHub. Viewed files stay collapsed" : PRReview.viewedPinnedHelp)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(Color(nsColor: .windowBackgroundColor))
        .overlay(alignment: .bottom) { Divider() }
        .overlay(alignment: .top) { Divider() }
    }
}

/// A file's comments that aren't on a line of this diff, listed at the top of the file.
struct DiffFileNotesView: View {
    let notes: DiffFileNotes
    let review: PRReview

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !notes.latestPush.isEmpty {
                Text(review.hasNewerCommits
                     ? "These threads refer to lines in the latest push. Your drafts hold the diff on \(review.shortHeadOid); submit or discard them to see the threads in place."
                     : "These threads refer to lines in a push that landed while the diff loaded. Reload to see them in place.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                ForEach(notes.latestPush, id: \.id) { DiffThreadView(thread: $0) }
            }
            ForEach(notes.outdated, id: \.id) { DiffThreadView(thread: $0) }
            ForEach(Array(notes.pending.enumerated()), id: \.offset) { _, comment in
                DiffPendingCard(comment: comment, location: location(of: comment), pullRequestURL: review.url)
            }
        }
        .padding(12)
        .frame(maxWidth: 820, alignment: .leading)
    }

    /// "Whole file", or the line and the commit the pending review was started on.
    private func location(of comment: PendingComment) -> String {
        guard let line = comment.line else { return "Whole file" }
        let commit = review.opened?.detail.pendingReview?.commitOid.prefix(7) ?? ""
        let lines = comment.startLine.map { $0 == line ? "Line \(line)" : "Lines \($0)–\(line)" } ?? "Line \(line)"
        return "\(lines) on \(commit.isEmpty ? "an earlier commit" : String(commit))"
    }
}

/// A line of explanation in place of a file's diff, with an optional action.
struct DiffNotice: View {
    let text: String
    var buttonTitle: String?
    var action: () -> Void = {}

    var body: some View {
        HStack(spacing: 12) {
            Text(text).foregroundStyle(.secondary)
            if let buttonTitle {
                Button(buttonTitle, action: action)
                    .help("Lay out this file's diff; very large files can make scrolling slower")
            }
        }
        .font(.callout)
        .padding(12)
        .frame(maxWidth: .infinity)
    }
}

/// Floats at the end of the selection: comment on it, or ask the agent about it.
struct DiffSelectionBar: View {
    let review: PRReview
    let canComment: Bool
    let comment: () -> Void
    @Environment(\.palette) private var palette

    var body: some View {
        HStack(spacing: 2) {
            Button(action: comment) {
                Label("Comment", systemImage: "text.bubble")
            }
            .disabled(!canComment)
            .help(canComment ? "Draft a review comment on the selected lines" : "Finish the open comment first")
            Divider().frame(height: 12)
            Button {
                review.chatFocusRequest = UUID()
            } label: {
                Label("Ask Agent", systemImage: "sparkles")
            }
            .help("Ask the agent in the chat about the selected lines")
        }
        .buttonStyle(.borderless)
        .tint(palette.selection)
        .font(.caption)
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(.regularMaterial, in: Capsule())
        .overlay(Capsule().strokeBorder(Color(nsColor: .separatorColor)))
        .shadow(color: .black.opacity(0.15), radius: 2, y: 1)
        .padding(.trailing, 8)
    }
}

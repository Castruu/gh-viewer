import AppKit
import SwiftUI

/// The threads, drafts and open editor under one diff line.
struct DiffLineComments: View {
    let review: PRReview
    let threads: [ReviewThread]
    let drafts: [DraftComment]
    /// Set when the new-comment composer belongs under this line.
    let composing: Selection?
    @Binding var editor: DiffEditorTarget?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(threads, id: \.id) { thread in
                DiffThreadView(thread: thread)
            }
            ForEach(drafts, id: \.id) { draft in
                if editor == .draft(id: draft.id) {
                    DiffCommentEditor(initialText: draft.body, submitLabel: "Save") { body in
                        // Keep the editor open if the save failed; the error banner says why.
                        if review.updateDraft(id: draft.id, body: body) { editor = nil }
                    } cancel: {
                        editor = nil
                    }
                } else {
                    DiffDraftCard(draft: draft, canEdit: editor == nil,
                                  edit: { editor = .draft(id: draft.id) },
                                  delete: { review.removeDraft(id: draft.id) })
                }
            }
            if let composing {
                DiffCommentEditor(initialText: "", submitLabel: "Comment") { body in
                    if review.addDraft(on: composing, body: body) {
                        editor = nil
                        review.selection = nil
                    }
                } cancel: {
                    editor = nil
                }
            }
        }
        .padding(.vertical, 8)
        .padding(.horizontal, 12)
        .frame(maxWidth: 820, alignment: .leading)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.background)
    }
}

/// An existing GitHub thread. A resolved one collapses to a line you can expand.
struct DiffThreadView: View {
    let thread: ReviewThread
    @State private var isExpanded = false

    var body: some View {
        DiffCard {
            if thread.isResolved && !isExpanded {
                Button {
                    isExpanded = true
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "checkmark.circle")
                        Text(summary).lineLimit(1)
                        Spacer(minLength: 0)
                        Image(systemName: "chevron.down")
                    }
                    .foregroundStyle(.secondary)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Show this resolved thread")
            } else {
                VStack(alignment: .leading, spacing: 10) {
                    if thread.isResolved || thread.isOutdated {
                        HStack(spacing: 6) {
                            if thread.isResolved { Label("Resolved", systemImage: "checkmark.circle") }
                            if thread.isOutdated { Label("Outdated", systemImage: "clock.arrow.circlepath") }
                            Spacer(minLength: 0)
                            if thread.isResolved {
                                Button("Hide") { isExpanded = false }
                                    .buttonStyle(.link)
                                    .help("Collapse this resolved thread")
                            }
                        }
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    }
                    ForEach(Array(thread.comments.enumerated()), id: \.offset) { index, comment in
                        if index > 0 { Divider() }
                        DiffThreadComment(comment: comment)
                    }
                }
            }
        }
    }

    private var summary: String {
        let first = thread.comments.first
        let body = DiffMarkdown.firstLine(of: first?.body ?? "")
        let replies = thread.comments.count > 1 ? " · \(countLabel(thread.comments.count - 1, "reply"))" : ""
        return "Resolved · \(first?.author ?? "ghost"): \(body)\(replies)"
    }
}

private struct DiffThreadComment: View {
    let comment: ThreadComment

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(comment.author ?? "ghost").fontWeight(.semibold)
                Text(relativeTime(comment.createdAt, now: .now)).foregroundStyle(.secondary)
                Spacer(minLength: 0)
                if let url = URL(string: comment.url) {
                    Link(destination: url) { Image(systemName: "arrow.up.right.square") }
                        .help("Open this comment on GitHub")
                }
            }
            .font(.callout)
            DiffMarkdown(text: comment.body)
        }
    }
}

/// A comment that hasn't been submitted. Agent drafts are marked so you check them first.
struct DiffDraftCard: View {
    let draft: DraftComment
    /// False while another editor is open, so its text isn't lost.
    let canEdit: Bool
    let edit: () -> Void
    let delete: () -> Void
    @Environment(\.palette) private var palette
    @State private var isConfirmingDelete = false

    var body: some View {
        DiffCard(tint: agentName == nil ? nil : palette.agent) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    if let agentName {
                        Label(agentName, systemImage: "sparkles")
                            .fontWeight(.semibold)
                            .foregroundStyle(palette.agent)
                        Text("Drafted by an agent. Check it before you submit.")
                            .foregroundStyle(.secondary)
                    } else {
                        Text("You").fontWeight(.semibold)
                    }
                    Text("Draft")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .overlay(Capsule().stroke(.secondary.opacity(0.6)))
                    if let lines {
                        Text(lines).foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                    Button("Edit", action: edit)
                        .disabled(!canEdit)
                        .help(canEdit ? "Edit this draft" : "Finish the open comment first")
                    Button("Delete") { isConfirmingDelete = true }
                        .help("Delete this draft")
                }
                .font(.callout)
                .buttonStyle(.link)
                DiffMarkdown(text: draft.body)
            }
        }
        .confirmationDialog("Delete this draft?", isPresented: $isConfirmingDelete) {
            Button("Delete Draft", role: .destructive, action: delete)
        } message: {
            Text("It hasn't been submitted, so it can't be recovered.")
        }
    }

    private var agentName: String? {
        if case .agent(let name) = draft.author { name } else { nil }
    }

    /// "Lines 10–14" for a multi-line draft.
    private var lines: String? {
        guard let start = draft.startLine, start != draft.line || draft.startSide != draft.side else { return nil }
        return "Lines \(start)–\(draft.line)"
    }
}

/// Inline editor for a new comment or a draft. ⌘↩ submits and Esc cancels.
struct DiffCommentEditor: View {
    let submitLabel: String
    let submit: (String) -> Void
    let cancel: () -> Void
    @State private var text: String
    @FocusState private var isFocused: Bool
    @State private var escapeMonitor: Any?

    init(initialText: String, submitLabel: String, submit: @escaping (String) -> Void, cancel: @escaping () -> Void) {
        self.submitLabel = submitLabel
        self.submit = submit
        self.cancel = cancel
        _text = State(initialValue: initialText)
    }

    var body: some View {
        DiffCard {
            VStack(alignment: .trailing, spacing: 8) {
                TextEditor(text: $text)
                    .font(.body)
                    .scrollContentBackground(.hidden)
                    .frame(minHeight: 64, maxHeight: 220)
                    .fixedSize(horizontal: false, vertical: true)
                    .focused($isFocused)
                    .overlay(alignment: .topLeading) {
                        if text.isEmpty {
                            Text("Leave a comment (Markdown)")
                                .foregroundStyle(.tertiary)
                                .padding(.leading, 5)
                                .allowsHitTesting(false)
                        }
                    }
                HStack {
                    Text("⌘↩ to \(submitLabel.lowercased()), Esc to cancel")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    // A shortcut rather than onKeyPress, which the text view swallows. Only while
                    // this editor has focus, so it doesn't fight the chat composer's.
                    Button("Cancel", action: cancel)
                        .help("Discard this text (Esc)")
                    Button(submitLabel, action: send)
                        .keyboardShortcut(isFocused ? KeyboardShortcut(.return, modifiers: .command) : nil)
                        .buttonStyle(.borderedProminent)
                        .disabled(trimmed.isEmpty)
                        .help("\(submitLabel) (⌘↩). Stays a local draft until you submit the review")
                }
                .controlSize(.small)
            }
        }
        .onAppear { isFocused = true }
        // NSTextView turns Esc into word completion before SwiftUI sees it, so catch it first.
        .onChange(of: isFocused, initial: true) { _, focused in
            if focused, escapeMonitor == nil {
                escapeMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
                    guard event.keyCode == 53, event.modifierFlags.intersection(.deviceIndependentFlagsMask).isEmpty
                    else { return event }
                    cancel()
                    return nil
                }
            } else if !focused {
                removeEscapeMonitor()
            }
        }
        .onDisappear(perform: removeEscapeMonitor)
    }

    private var trimmed: String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func removeEscapeMonitor() {
        if let escapeMonitor { NSEvent.removeMonitor(escapeMonitor) }
        escapeMonitor = nil
    }

    private func send() {
        guard !trimmed.isEmpty else { return }
        submit(trimmed)
    }
}

/// The rounded box comments and editors sit in; a tint marks agent drafts.
struct DiffCard<Content: View>: View {
    var tint: Color?
    @ViewBuilder let content: Content

    var body: some View {
        content
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(tint?.opacity(0.07) ?? Color.clear, in: RoundedRectangle(cornerRadius: 6))
            .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
            .overlay {
                RoundedRectangle(cornerRadius: 6)
                    .strokeBorder(tint?.opacity(0.7) ?? Color(nsColor: .separatorColor), lineWidth: tint == nil ? 1 : 1.5)
            }
    }
}

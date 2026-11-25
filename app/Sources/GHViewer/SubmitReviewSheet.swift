import SwiftUI

/// Submits the drafts as one review: pick the event, write a summary, check what goes out.
struct SubmitReviewSheet: View {
    let review: PRReview
    /// Called with the review's URL once GitHub accepted it.
    let submitted: (String) -> Void
    @State private var event = ReviewEvent.comment
    @State private var message = ""
    @Environment(\.dismiss) private var dismiss
    @Environment(\.palette) private var palette

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Submit Review").font(.title3.weight(.semibold))

            Picker("Event", selection: $event) {
                Text("Comment").tag(ReviewEvent.comment)
                Text("Approve").tag(ReviewEvent.approve)
                Text("Request Changes").tag(ReviewEvent.requestChanges)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .help("Comment without a verdict, approve, or request changes")

            TextEditor(text: $message)
                .font(.body)
                .frame(height: 100)
                .scrollContentBackground(.hidden)
                .padding(4)
                .background(.background, in: RoundedRectangle(cornerRadius: 5))
                .overlay(RoundedRectangle(cornerRadius: 5).strokeBorder(Color(nsColor: .separatorColor)))
                .overlay(alignment: .topLeading) {
                    if message.isEmpty {
                        Text("Summary (optional, Markdown)")
                            .foregroundStyle(.tertiary)
                            .padding(.leading, 9)
                            .padding(.top, 4)
                            .allowsHitTesting(false)
                    }
                }

            SubmitDraftSummary(drafts: review.drafts)

            if let pending = review.pendingCommentCount, pending > 0 {
                Label("Also submits \(countLabel(pending, "comment")) you started on github.com.", systemImage: "info.circle")
                    .foregroundStyle(.secondary)
            }
            if review.hasNewerCommits {
                Label("Submits on \(review.shortHeadOid), the commit your drafts were written on, not the latest push.",
                      systemImage: "pin")
                    .foregroundStyle(.secondary)
            }
            if let error = review.actionError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(palette.fail)
                    .textSelection(.enabled)
            }

            HStack {
                if review.isSubmitting {
                    ProgressView().controlSize(.small)
                    Text("Submitting…").foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(review.isSubmitting)
                    .help("Close without submitting; your drafts stay")
                Button("Submit Review") { send() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(review.isSubmitting || isEmpty)
                    .help(isEmpty ? "Write a summary or add comments first" : "Send this review to GitHub")
            }
        }
        .font(.callout)
        .padding(20)
        .frame(width: 520)
        // An error from an earlier action doesn't belong to this submit.
        .onAppear { review.actionError = nil }
    }

    /// GitHub only accepts an approval without a body or comments.
    private var isEmpty: Bool {
        event != .approve && message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && review.drafts.isEmpty && (review.pendingCommentCount ?? 0) == 0
    }

    private func send() {
        review.actionError = nil
        Task {
            if let url = await review.submit(event: event, body: message.trimmingCharacters(in: .whitespacesAndNewlines)) {
                submitted(url)
                dismiss()
            }
        }
    }
}

/// The drafts that go out with the review: yours, then each agent's, so it's clear who wrote
/// what you're about to post (decision 5c). Each group is listed by file.
private struct SubmitDraftSummary: View {
    let drafts: [DraftComment]
    @Environment(\.palette) private var palette

    var body: some View {
        if drafts.isEmpty {
            Text("No draft comments. The review is submitted with the summary only.")
                .foregroundStyle(.secondary)
        } else {
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(groups, id: \.agent) { agent, drafts in
                        VStack(alignment: .leading, spacing: 6) {
                            header(agent: agent, count: drafts.count)
                            ForEach(Dictionary(grouping: drafts, by: \.path).sorted { $0.key < $1.key }, id: \.key) { path, drafts in
                                VStack(alignment: .leading, spacing: 3) {
                                    Text(verbatim: path).fontWeight(.semibold).lineLimit(1).truncationMode(.head)
                                    ForEach(drafts, id: \.id) { row($0) }
                                }
                            }
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
            }
            .frame(maxHeight: 220)
            .fixedSize(horizontal: false, vertical: true)
            .background(.background.secondary, in: RoundedRectangle(cornerRadius: 5))
        }
    }

    /// Your drafts first (`agent` nil), then each agent's by name.
    private var groups: [(agent: String?, drafts: [DraftComment])] {
        Dictionary(grouping: drafts) { draft -> String? in
            if case .agent(let name) = draft.author { name } else { nil }
        }
        .sorted { ($0.key ?? "") < ($1.key ?? "") }
        .map { ($0.key, $0.value) }
    }

    @ViewBuilder
    private func header(agent: String?, count: Int) -> some View {
        if let agent {
            Label("Drafted by \(agent) (\(count))", systemImage: "sparkles")
                .font(.headline)
                .foregroundStyle(palette.agent)
        } else {
            Text("Your comments (\(count))").font(.headline)
        }
    }

    /// "L7", "L7–9", or "old L7" for a removed line, as GitHub numbers the left side.
    private func lines(_ draft: DraftComment) -> String {
        let range = draft.startLine.map { $0 == draft.line ? "L\(draft.line)" : "L\($0)–\(draft.line)" } ?? "L\(draft.line)"
        return draft.side == .left ? "old \(range)" : range
    }

    private func row(_ draft: DraftComment) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text(verbatim: lines(draft))
                .monospacedDigit()
                .foregroundStyle(.secondary)
                .frame(minWidth: 52, alignment: .leading)
            Text(verbatim: DiffMarkdown.firstLine(of: draft.body))
                .lineLimit(1)
        }
    }
}

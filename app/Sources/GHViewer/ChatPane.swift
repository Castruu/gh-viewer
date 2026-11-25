import SwiftUI

/// The agent chat for the selected PR, in the window's inspector.
struct ChatPane: View {
    /// `nil` when no PR is selected.
    let review: PRReview?

    var body: some View {
        if let review {
            ChatConversation(review: review)
                // Fresh composer and scroll state per PR.
                .id(review.url)
        } else {
            ContentUnavailableView {
                Label("Agent Chat", systemImage: "bubble.left.and.bubble.right")
            } description: {
                Text("Select a pull request to ask your coding agent about it.")
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

private struct ChatConversation: View {
    let review: PRReview

    var body: some View {
        VStack(spacing: 0) {
            ChatHeader(review: review)
            Divider()
            if review.chat.items.isEmpty {
                ChatSuggestions(review: review)
            } else {
                ChatTranscript(review: review)
            }
            Divider()
            ChatComposer(review: review)
        }
    }
}

/// The agent picker, what the agent is doing, and Clear Chat.
private struct ChatHeader: View {
    @Bindable var review: PRReview
    @State private var isConfirmingClear = false
    @Environment(\.palette) private var palette

    var body: some View {
        let chat = review.chat
        HStack(spacing: 8) {
            Picker("Agent", selection: $review.chat.agent) {
                Text("Claude").tag(AgentKind.claude)
                Text("Codex").tag(AgentKind.codex)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .disabled(chat.isRunning)
            .help(chat.isRunning ? "You can switch agents once this answer is done"
                                 : "The agent your next question goes to")
            Spacer(minLength: 0)
            if chat.isRunning {
                HStack(spacing: 6) {
                    ProgressView()
                        .controlSize(.small)
                    if review.isCancelPending {
                        Text("Stopping…")
                    } else if chat.isStarting {
                        ChatStartingText(agent: chat.agent)
                    } else {
                        Text("Working…")
                    }
                }
                .lineLimit(1)
                .font(.callout)
                .foregroundStyle(palette.agent)
                .help(chat.isStarting ? "The first start downloads the agent and can take a minute"
                                      : "The agent is working on your question")
            } else {
                Button("Clear Chat", systemImage: "trash") {
                    isConfirmingClear = true
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
                .disabled(chat.items.isEmpty)
                .help("Clear Chat")
                .confirmationDialog("Clear this chat?", isPresented: $isConfirmingClear) {
                    Button("Clear Chat", role: .destructive, action: review.clearChat)
                } message: {
                    Text("The conversation is deleted, and the next question starts a new session.")
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

/// "Starting Claude…", with the seconds once it takes a while.
private struct ChatStartingText: View {
    let agent: AgentKind
    @State private var start = Date.now

    var body: some View {
        TimelineView(.periodic(from: start, by: 1)) { context in
            let seconds = Int(context.date.timeIntervalSince(start))
            Text(seconds < 5 ? "Starting \(agent.displayName)…" : "Starting \(agent.displayName)… \(seconds) s")
        }
    }
}

/// Before the first question: a few that work on any PR, sent with one click.
private struct ChatSuggestions: View {
    let review: PRReview
    @Environment(\.palette) private var palette

    private static let questions = [
        "Summarize this PR",
        "What could break?",
        "Review the changes and draft comments for anything that needs fixing",
    ]

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "bubble.left.and.bubble.right")
                .font(.largeTitle)
                .foregroundStyle(palette.agent)
            Text("Ask \(review.chat.agent.displayName) about #\(review.number)")
                .font(.headline)
            Text("Select lines in the diff to ask about them, or start with one of these.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            VStack(spacing: 6) {
                ForEach(Self.questions, id: \.self) { question in
                    Button {
                        // About the whole PR, so a leftover line selection doesn't narrow it.
                        Task { await review.ask(question, withSelection: false) }
                    } label: {
                        Text(question)
                            .multilineTextAlignment(.leading)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 7)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(palette.accent.opacity(0.1), in: .rect(cornerRadius: 8))
                            .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .help(question == Self.questions.last
                          ? "The agent drafts review comments, which show in the diff for you to edit or discard"
                          : "Ask this now")
                }
            }
            .padding(.top, 6)
        }
        .padding(20)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// The question field. ↩ sends and ⌥↩ adds a line, as in a macOS text field.
private struct ChatComposer: View {
    let review: PRReview
    @State private var text = ""
    /// Turned off with the chip's ×, for the next question only.
    @State private var includesSelection = true
    @FocusState private var isFocused: Bool
    @Environment(\.palette) private var palette

    var body: some View {
        let chat = review.chat
        VStack(alignment: .leading, spacing: 6) {
            if let selection = review.selection, includesSelection {
                ChatSelectionChip(label: "Includes \(selection.chatLabel)") { includesSelection = false }
            }
            HStack(alignment: .bottom, spacing: 6) {
                TextField("Ask \(chat.agent.displayName) about this PR", text: $text, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(1...8)
                    .focused($isFocused)
                    .onSubmit(send)
                    .padding(.vertical, 2)
                if chat.isRunning {
                    Button("Stop", systemImage: "stop.circle.fill", action: review.cancel)
                        .keyboardShortcut(".", modifiers: .command)
                        .disabled(review.isCancelPending)
                        .help("Stop the agent (⌘.)")
                } else {
                    Button("Send", systemImage: "arrow.up.circle.fill", action: send)
                        .disabled(isEmpty)
                        .help("Send (↩). ⌥↩ adds a new line.")
                }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.borderless)
            .imageScale(.large)
            .padding(8)
            .background(.background, in: .rect(cornerRadius: 8))
            .overlay {
                RoundedRectangle(cornerRadius: 8)
                    .strokeBorder(isFocused ? palette.accent : Color.secondary.opacity(0.3))
            }
        }
        .padding(10)
        .onChange(of: review.selection) { includesSelection = true }
        // Also runs on appear, for an "Ask Agent" that opened the inspector.
        .task(id: review.chatFocusRequest) {
            if review.chatFocusRequest != nil {
                isFocused = true
            }
        }
    }

    private var isEmpty: Bool {
        text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func send() {
        guard !review.chat.isRunning, !isEmpty else { return }
        let question = text
        let withSelection = includesSelection
        text = ""
        includesSelection = true
        Task { await review.ask(question, withSelection: withSelection) }
    }
}

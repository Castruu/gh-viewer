import SwiftUI

/// The conversation. Follows the bottom while the agent streams, unless you scrolled up.
struct ChatTranscript: View {
    let review: PRReview
    @State private var isAtBottom = true
    @State private var viewportHeight: CGFloat = 0
    private let bottom = "bottom"

    var body: some View {
        let chat = review.chat
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    ForEach(chat.items) { item in
                        ChatItemRow(item: item, isRunning: chat.isRunning, review: review)
                    }
                    Color.clear
                        .frame(height: 1)
                        .id(bottom)
                        .onGeometryChange(for: Bool.self) { geometry in
                            geometry.frame(in: .scrollView).minY <= viewportHeight + 24
                        } action: { isAtBottom = $0 }
                }
                .padding(12)
            }
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { viewportHeight = $0 }
            .onAppear { proxy.scrollTo(bottom, anchor: .bottom) }
            .onChange(of: chat.items.count) {
                // Your own question always brings you back to the bottom.
                if isAtBottom || chat.items.last?.isUser == true {
                    proxy.scrollTo(bottom, anchor: .bottom)
                }
            }
            .onChange(of: chat.items.last?.streamedLength) {
                if isAtBottom {
                    proxy.scrollTo(bottom, anchor: .bottom)
                }
            }
        }
    }
}

/// One transcript entry. Takes plain values, so rows whose item didn't change skip their body.
private struct ChatItemRow: View {
    let item: ChatItem
    let isRunning: Bool
    let review: PRReview

    var body: some View {
        switch item.kind {
        case .user(let text, let selection, let agent):
            ChatUserMessage(text: text, selection: selection, agent: agent)
        case .agent(let markdown):
            ChatAgentMessage(markdown: markdown)
        case .thought(let text):
            ChatThought(text: text)
        case .tool(_, let title, let kind, let status, let output):
            ChatToolRow(title: title, kind: kind, status: status, output: output)
        case .plan(let entries):
            ChatPlan(entries: entries)
        case .permission(let requestId, let title, let toolTitle, let options, let answer):
            // Unanswered once the turn is over: the core already answered it as cancelled.
            ChatPermissionCard(title: title, toolTitle: toolTitle, options: options, answer: answer,
                               isOpen: isRunning) { optionId in
                review.respond(toPermission: requestId, optionId: optionId)
            }
        case .notice(let text):
            ChatNotice(text: text, isError: item.isErrorNotice)
        }
    }
}

/// A pill with the selected lines, e.g. "src/a.rs:11–14".
struct ChatSelectionChip: View {
    let label: String
    var remove: (() -> Void)?
    @Environment(\.palette) private var palette

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "text.line.first.and.arrowtriangle.forward")
            Text(label)
                .lineLimit(1)
                .truncationMode(.middle)
            if let remove {
                Button(action: remove) {
                    Image(systemName: "xmark")
                        .font(.caption2.weight(.bold))
                }
                .buttonStyle(.borderless)
                .help("Don't include these lines in this question")
            }
        }
        .font(.caption)
        .foregroundStyle(palette.accent)
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(palette.accent.opacity(0.12), in: Capsule())
        .help(label)
    }
}

private struct ChatUserMessage: View {
    let text: String
    let selection: Selection?
    let agent: AgentKind
    @Environment(\.palette) private var palette

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(text)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 6) {
                Text("To \(agent.displayName)")
                    .font(.caption)
                    .foregroundStyle(palette.agent)
                if let selection {
                    ChatSelectionChip(label: selection.chatLabel)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(palette.accent.opacity(0.1), in: .rect(cornerRadius: 8))
    }
}

/// The agent's reply, marked by a rule in its color.
private struct ChatAgentMessage: View {
    let markdown: String
    @Environment(\.palette) private var palette

    var body: some View {
        ChatMarkdown(markdown: markdown)
            .padding(.leading, 10)
            .overlay(alignment: .leading) {
                Capsule()
                    .fill(palette.agent.opacity(0.6))
                    .frame(width: 2)
            }
    }
}

private struct ChatThought: View {
    let text: String

    var body: some View {
        DisclosureGroup {
            Text(text)
                .font(.callout)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        } label: {
            Text("Thinking")
                .foregroundStyle(.secondary)
        }
        .disclosureGroupStyle(ChatDisclosureStyle())
    }
}

/// A tool the agent ran: status, title and kind. Click to see its output.
private struct ChatToolRow: View {
    let title: String
    let kind: String
    let status: ToolStatus
    let output: String?

    var body: some View {
        if let output, !output.isEmpty {
            DisclosureGroup {
                ChatToolOutput(output: output, isDiff: kind == "edit")
            } label: {
                label
            }
            .disclosureGroupStyle(ChatDisclosureStyle())
        } else {
            label
                // Lines up with the rows that have a disclosure chevron.
                .padding(.leading, 16)
        }
    }

    private var label: some View {
        HStack(spacing: 6) {
            ChatToolStatusIcon(status: status)
            Text(title)
                .lineLimit(1)
                .truncationMode(.middle)
                .help(title)
            Spacer(minLength: 4)
            Text(kind)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .font(.callout)
    }
}

private struct ChatToolStatusIcon: View {
    let status: ToolStatus
    @Environment(\.palette) private var palette

    var body: some View {
        Group {
            switch status {
            case .pending, .inProgress:
                ProgressView()
                    .controlSize(.mini)
            case .completed:
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(palette.pass)
            case .failed:
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(palette.fail)
            }
        }
        .frame(width: 14)
        .help(label)
    }

    private var label: String {
        switch status {
        case .pending: "Pending"
        case .inProgress: "Running"
        case .completed: "Done"
        case .failed: "Failed"
        }
    }
}

/// Monospaced output, as tall as its text up to a cap, then scrolling.
private struct ChatToolOutput: View {
    let output: String
    /// Colors `+`/`-` lines, which is how the core renders an edit's diff.
    let isDiff: Bool
    @State private var height: CGFloat = 0
    @Environment(\.palette) private var palette

    var body: some View {
        ScrollView {
            Text(text)
                .font(.system(.caption, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        }
        .frame(height: min(height, 220))
        .background(.quaternary.opacity(0.6), in: .rect(cornerRadius: 6))
        .padding(.leading, 16)
    }

    private var text: AttributedString {
        guard isDiff else { return AttributedString(output) }
        var text = AttributedString()
        for (index, line) in output.split(separator: "\n", omittingEmptySubsequences: false).enumerated() {
            var part = AttributedString((index == 0 ? "" : "\n") + line)
            if line.hasPrefix("+") {
                part.foregroundColor = palette.added
            } else if line.hasPrefix("-") {
                part.foregroundColor = palette.removed
            }
            text += part
        }
        return text
    }
}

private struct ChatPlan: View {
    let entries: [PlanEntry]
    @Environment(\.palette) private var palette

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Plan")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ForEach(Array(entries.enumerated()), id: \.offset) { _, entry in
                Label {
                    Text(entry.content)
                        .foregroundStyle(entry.status == "completed" ? .secondary : .primary)
                        .fixedSize(horizontal: false, vertical: true)
                } icon: {
                    icon(entry.status)
                }
                .font(.callout)
            }
        }
    }

    @ViewBuilder
    private func icon(_ status: String) -> some View {
        switch status {
        case "completed":
            Image(systemName: "checkmark.circle.fill").foregroundStyle(palette.pass)
        case "in_progress":
            Image(systemName: "circle.lefthalf.filled").foregroundStyle(palette.pending)
        default:
            Image(systemName: "circle").foregroundStyle(.secondary)
        }
    }
}

/// The agent asks before running a tool. Shows its own options until answered, then one line.
private struct ChatPermissionCard: View {
    let title: String
    let toolTitle: String?
    let options: [PermissionChoice]
    let answer: String?
    /// False once the turn is over; nobody is waiting for an answer then.
    let isOpen: Bool
    let respond: (String?) -> Void
    @Environment(\.palette) private var palette

    var body: some View {
        if let answer {
            let kind = options.first { $0.name == answer }?.kind ?? ""
            Label {
                Text("\(title): \(kind == "allow_once" ? "Allowed once" : answer)")
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            } icon: {
                Image(systemName: kind.hasPrefix("allow") ? "checkmark.shield" : "xmark.shield")
                    .foregroundStyle(kind.hasPrefix("allow") ? palette.pass : palette.fail)
            }
            .font(.callout)
            .help(title)
        } else if !isOpen {
            Label("\(title): not answered", systemImage: "shield.slash")
                .font(.callout)
                .foregroundStyle(.secondary)
                .lineLimit(2)
                .help(title)
        } else {
            VStack(alignment: .leading, spacing: 8) {
                Label("Permission needed", systemImage: "hand.raised.fill")
                    .font(.headline)
                    .foregroundStyle(palette.pending)
                Text(title)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                if let toolTitle, toolTitle != title {
                    Text(toolTitle)
                        .font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                ViewThatFits(in: .horizontal) {
                    HStack { buttons }
                    VStack(alignment: .leading) { buttons }
                }
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(palette.pending.opacity(0.1), in: .rect(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(palette.pending.opacity(0.5)))
        }
    }

    @ViewBuilder
    private var buttons: some View {
        ForEach(options, id: \.id) { option in
            let allows = option.kind.hasPrefix("allow")
            Button(option.name) { respond(option.id) }
                .buttonStyle(.borderedProminent)
                .tint(allows ? palette.pass : palette.fail)
                .help(allows ? "Let the agent run this" : "Don't let the agent run this")
        }
        Button("Dismiss") { respond(nil) }
            .help("Answer neither; the agent treats the request as cancelled")
    }
}

private struct ChatNotice: View {
    let text: String
    let isError: Bool
    @Environment(\.palette) private var palette

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if isError {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(palette.fail)
            }
            Text(ChatMarkdown.inline(text))
                .foregroundStyle(isError ? .primary : .secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(.callout)
    }
}

/// A disclosure whose whole label toggles it, unlike the macOS default where only the chevron does.
private struct ChatDisclosureStyle: DisclosureGroupStyle {
    func makeBody(configuration: Configuration) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Button {
                configuration.isExpanded.toggle()
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .rotationEffect(.degrees(configuration.isExpanded ? 90 : 0))
                        .frame(width: 10)
                    configuration.label
                }
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            if configuration.isExpanded {
                configuration.content
            }
        }
    }
}

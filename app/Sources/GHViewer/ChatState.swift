import Foundation

/// The agent conversation for one PR, built from the core's `AgentEvent`s.
struct ChatState {
    /// The agent the next question goes to.
    var agent: AgentKind = .claude
    /// One session per agent kind; it lives as long as the agent process.
    var sessions: [AgentKind: String] = [:]
    private(set) var items: [ChatItem] = []
    /// True from sending a question until the agent's turn ends.
    var isRunning = false
    /// True while the agent process and session start; the first `npx` run can take a while.
    var isStarting = false

    func ownsSession(_ sessionId: String) -> Bool {
        sessions.values.contains(sessionId)
    }

    mutating func append(_ kind: ChatItem.Kind) {
        items.append(ChatItem(kind: kind))
    }

    /// Puts back a saved transcript, e.g. after a restart.
    mutating func restore(_ saved: [ChatItem]) {
        items = saved
    }

    /// Empties the transcript and forgets the sessions, so the next question starts a fresh one.
    mutating func clear() {
        items = []
        sessions = [:]
    }

    mutating func apply(_ event: AgentEvent, sessionId: String) {
        switch event {
        case .messageChunk(let text):
            appendText(text, thought: false)
        case .thoughtChunk(let text):
            appendText(text, thought: true)
        case .toolCall(let id, let title, let kind, let status):
            append(.tool(id: id, title: title, kind: kind, status: status, output: nil))
        case .toolCallUpdate(let id, let title, let status, let text):
            guard let index = items.lastIndex(where: { $0.toolId == id }),
                  case .tool(_, let oldTitle, let kind, let oldStatus, let oldOutput) = items[index].kind
            else { return }
            items[index].kind = .tool(id: id, title: title ?? oldTitle, kind: kind,
                                      status: status ?? oldStatus, output: text ?? oldOutput)
        case .plan(let entries):
            // Agents resend the whole plan on each change; keep one plan per turn.
            if let index = items.lastIndex(where: \.isPlan), !items[index...].contains(where: \.isUser) {
                items[index].kind = .plan(entries)
            } else {
                append(.plan(entries))
            }
        case .permissionRequest(let requestId, let toolCallId, let title, let options):
            let toolTitle = items.last { $0.toolId == toolCallId }?.toolTitle
            append(.permission(requestId: requestId, title: title, toolTitle: toolTitle, options: options, answer: nil))
        case .disconnected(let message):
            if let kind = sessions.first(where: { $0.value == sessionId })?.key {
                sessions[kind] = nil
            }
            isRunning = false
            append(.notice(message))
        }
    }

    mutating func resolvePermission(_ requestId: String, optionId: String?) {
        guard let index = items.lastIndex(where: { $0.permissionRequestId == requestId }),
              case .permission(_, let title, let toolTitle, let options, _) = items[index].kind
        else { return }
        let answer = options.first { $0.id == optionId }?.name ?? "Dismissed"
        items[index].kind = .permission(requestId: requestId, title: title, toolTitle: toolTitle,
                                        options: options, answer: answer)
    }

    /// Streams into the last message of the same kind instead of one bubble per chunk.
    private mutating func appendText(_ text: String, thought: Bool) {
        if let last = items.indices.last {
            switch items[last].kind {
            case .agent(let existing) where !thought:
                items[last].kind = .agent(existing + text)
                return
            case .thought(let existing) where thought:
                items[last].kind = .thought(existing + text)
                return
            default:
                break
            }
        }
        append(thought ? .thought(text) : .agent(text))
    }
}

struct ChatItem: Identifiable {
    enum Kind {
        case user(text: String, selection: Selection?, agent: AgentKind)
        /// Markdown from the agent, growing as chunks stream in.
        case agent(String)
        case thought(String)
        case tool(id: String, title: String, kind: String, status: ToolStatus, output: String?)
        case plan([PlanEntry])
        /// `toolTitle` is the title of the tool call it asks about, when known.
        /// `answer` is the chosen option's name once answered.
        case permission(requestId: String, title: String, toolTitle: String?, options: [PermissionChoice], answer: String?)
        /// Errors and disconnects.
        case notice(String)
        /// Informational, e.g. where a restored conversation ends.
        case info(String)
        /// You stopped the turn.
        case stopped
    }

    let id = UUID()
    var kind: Kind

    var toolId: String? {
        if case .tool(let id, _, _, _, _) = kind { id } else { nil }
    }

    var toolTitle: String? {
        if case .tool(_, let title, _, _, _) = kind { title } else { nil }
    }

    var permissionRequestId: String? {
        if case .permission(let requestId, _, _, _, _) = kind { requestId } else { nil }
    }

    var isPlan: Bool {
        if case .plan = kind { true } else { false }
    }

    var isUser: Bool {
        if case .user = kind { true } else { false }
    }

    /// The agent a question went to.
    var userAgent: AgentKind? {
        if case .user(_, _, let agent) = kind { agent } else { nil }
    }
}

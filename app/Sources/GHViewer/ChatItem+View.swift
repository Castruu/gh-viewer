import Foundation

// Display helpers for the chat views.

extension AgentKind {
    var displayName: String {
        switch self {
        case .claude: "Claude"
        case .codex: "Codex"
        }
    }
}

extension Selection {
    /// "src/a.rs:11–14", or "src/a.rs:11" for a single line.
    var chatLabel: String {
        startLine == line && startSide == side ? "\(path):\(line)" : "\(path):\(startLine)–\(line)"
    }
}

extension ChatItem {
    /// Grows as the item streams, so the transcript can follow the bottom without diffing the text.
    var streamedLength: Int {
        switch kind {
        case .agent(let text), .thought(let text): text.utf8.count
        case .tool(_, _, _, _, let output): output?.utf8.count ?? 0
        case .plan(let entries): entries.count
        case .permission(_, _, _, _, let answer): answer == nil ? 0 : 1
        case .user, .notice, .info, .stopped: 0
        }
    }
}

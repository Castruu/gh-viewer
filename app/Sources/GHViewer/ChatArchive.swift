import Foundation

/// Saves each PR's chat transcript as JSON in `chats/<owner>/<name>/<number>.json`, so it
/// shows again after a restart. Only the transcript survives; agent sessions don't.
struct ChatArchive {
    /// …/GHViewer/chats
    let root: URL
    /// Older items are dropped beyond this, which keeps files small and launches quick.
    static let maxItems = 500

    struct Saved {
        let items: [ChatItem]
        let savedAt: Date
    }

    /// The saved transcript, or nil if there is none. A file that doesn't decode is moved
    /// aside to `.corrupt`, so a later save can't overwrite it, and reported as an error.
    func load(repo: String, number: UInt64) throws -> Saved? {
        let url = fileURL(repo: repo, number: number)
        guard let data = try? Data(contentsOf: url) else { return nil }
        do {
            let file = try Self.decoder.decode(ArchivedChat.self, from: data)
            return Saved(items: file.items.map { ChatItem(kind: $0.kind) }, savedAt: file.savedAt)
        } catch {
            let aside = url.appendingPathExtension("corrupt")
            try? FileManager.default.removeItem(at: aside)
            try FileManager.default.moveItem(at: url, to: aside)
            throw ArchiveError(errorDescription: "The saved conversation couldn't be read, so it was moved to \(aside.path).")
        }
    }

    func save(_ items: [ChatItem], repo: String, number: UInt64) throws {
        let url = fileURL(repo: repo, number: number)
        let file = ArchivedChat(savedAt: .now, items: items.suffix(Self.maxItems).map { ArchivedItem($0.kind) })
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        // `.atomic` writes a temporary file and renames it over the old one.
        try Self.encoder.encode(file).write(to: url, options: .atomic)
    }

    func delete(repo: String, number: UInt64) throws {
        do {
            try FileManager.default.removeItem(at: fileURL(repo: repo, number: number))
        } catch CocoaError.fileNoSuchFile {
            // Nothing saved yet.
        }
    }

    /// Lowercased like the drafts store: the disk ignores case, and GitHub names don't.
    private func fileURL(repo: String, number: UInt64) -> URL {
        root.appending(path: "\(repo.lowercased())/\(number).json")
    }

    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        return encoder
    }()

    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }()
}

private struct ArchiveError: LocalizedError {
    let errorDescription: String?
}

private struct ArchivedChat: Codable {
    let savedAt: Date
    let items: [ArchivedItem]
}

/// `ChatItem.Kind` in Codable form, since the core's generated types aren't Codable.
private enum ArchivedItem: Codable {
    case user(text: String, selection: ArchivedSelection?, agent: String)
    case agent(text: String)
    case thought(text: String)
    case tool(id: String, title: String, kind: String, status: String, output: String?)
    case plan(entries: [ArchivedPlanEntry])
    case permission(requestId: String, title: String, toolTitle: String?, options: [ArchivedChoice], answer: String?)
    case notice(text: String)
    case info(text: String)
    case stopped

    init(_ kind: ChatItem.Kind) {
        switch kind {
        case .user(let text, let selection, let agent):
            self = .user(text: text, selection: selection.map(ArchivedSelection.init), agent: agent == .codex ? "codex" : "claude")
        case .agent(let text):
            self = .agent(text: text)
        case .thought(let text):
            self = .thought(text: text)
        case .tool(let id, let title, let kind, let status, let output):
            let status = switch status {
            case .pending: "pending"
            case .inProgress: "in_progress"
            case .completed: "completed"
            case .failed: "failed"
            }
            self = .tool(id: id, title: title, kind: kind, status: status, output: output)
        case .plan(let entries):
            self = .plan(entries: entries.map { ArchivedPlanEntry(content: $0.content, status: $0.status) })
        case .permission(let requestId, let title, let toolTitle, let options, let answer):
            self = .permission(requestId: requestId, title: title, toolTitle: toolTitle,
                               options: options.map { ArchivedChoice(id: $0.id, name: $0.name, kind: $0.kind) },
                               answer: answer)
        case .notice(let text):
            self = .notice(text: text)
        case .info(let text):
            self = .info(text: text)
        case .stopped:
            self = .stopped
        }
    }

    var kind: ChatItem.Kind {
        switch self {
        case .user(let text, let selection, let agent):
            .user(text: text, selection: selection?.selection, agent: agent == "codex" ? .codex : .claude)
        case .agent(let text):
            .agent(text)
        case .thought(let text):
            .thought(text)
        case .tool(let id, let title, let kind, let status, let output):
            // A call still running when the app quit never finished.
            .tool(id: id, title: title, kind: kind, status: status == "completed" ? .completed : .failed, output: output)
        case .plan(let entries):
            .plan(entries.map { PlanEntry(content: $0.content, status: $0.status) })
        case .permission(let requestId, let title, let toolTitle, let options, let answer):
            // History only: the request is gone, and its id must never answer a new one.
            .permission(requestId: requestId, title: title, toolTitle: toolTitle,
                        options: options.map { PermissionChoice(id: $0.id, name: $0.name, kind: $0.kind) },
                        answer: answer ?? "Not answered")
        case .notice(let text):
            .notice(text)
        case .info(let text):
            .info(text)
        case .stopped:
            .stopped
        }
    }
}

private struct ArchivedSelection: Codable {
    let path: String
    let startLine: UInt32
    let startSide: String
    let line: UInt32
    let side: String

    init(_ selection: Selection) {
        path = selection.path
        startLine = selection.startLine
        startSide = selection.startSide == .left ? "LEFT" : "RIGHT"
        line = selection.line
        side = selection.side == .left ? "LEFT" : "RIGHT"
    }

    var selection: Selection {
        Selection(path: path, startLine: startLine, startSide: startSide == "LEFT" ? .left : .right,
                  line: line, side: side == "LEFT" ? .left : .right)
    }
}

private struct ArchivedPlanEntry: Codable {
    let content: String
    let status: String
}

private struct ArchivedChoice: Codable {
    let id: String
    let name: String
    let kind: String
}

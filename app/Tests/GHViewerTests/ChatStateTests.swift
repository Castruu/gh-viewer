import Testing
@testable import GHViewer

struct ChatStateTests {
    @Test func onlyTheRunningSessionsDisconnectEndsTheTurn() {
        var chat = ChatState()
        chat.sessions = [.claude: "claude-1", .codex: "codex-1"]
        chat.isRunning = true
        chat.runningSession = "codex-1"

        chat.apply(.disconnected(message: "Claude stopped."), sessionId: "claude-1")
        #expect(chat.isRunning)
        #expect(chat.sessions[.claude] == nil)

        chat.apply(.disconnected(message: "Codex stopped."), sessionId: "codex-1")
        #expect(!chat.isRunning)
    }

    @Test func aTurnsUnansweredPermissionsCloseWhenItEnds() {
        var chat = ChatState()
        chat.isRunning = true
        chat.runningSession = "claude-1"
        let options = [PermissionChoice(id: "yes", name: "Allow", kind: "allow_once")]
        for id in ["permission-1", "permission-2"] {
            chat.apply(.permissionRequest(requestId: id, toolCallId: "t", title: "Run tests", options: options),
                       sessionId: "claude-1")
        }
        chat.resolvePermission("permission-1", optionId: "yes")

        chat.endTurn()
        #expect(!chat.isRunning && chat.runningSession == nil)
        let answers = chat.items.compactMap { item -> String? in
            guard case .permission(_, _, _, _, let answer) = item.kind else { return nil }
            return answer
        }
        #expect(answers == ["Allow", "Not answered"])
    }
}

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
}

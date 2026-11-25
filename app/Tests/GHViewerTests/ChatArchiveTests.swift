import Foundation
import Testing
@testable import GHViewer

struct ChatArchiveTests {
    @Test func savesOnlyTheEndOfLongToolOutput() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let archive = ChatArchive(root: root)
        let output = String(repeating: "x", count: 100_000) + "END"
        try archive.save([ChatItem(kind: .tool(id: "t", title: "Build", kind: "execute", status: .completed, output: output))],
                         repo: "o/r", number: 7)

        let saved = try #require(try archive.load(repo: "o/r", number: 7)?.items.first)
        guard case .tool(_, _, _, _, let restored?) = saved.kind else { Issue.record("not a tool call"); return }
        #expect(restored.count == ChatArchive.maxOutputLength + 1)
        #expect(restored.hasPrefix("…") && restored.hasSuffix("END"))
    }
}

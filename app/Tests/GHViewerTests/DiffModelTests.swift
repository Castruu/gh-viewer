import Testing
@testable import GHViewer

struct DiffModelTests {
    /// Rows: 0 hunk, 1 line 1, 2 line 2, 3 hunk, 4 line 20.
    private let file = DiffFileModel(
        DiffFile(path: "a.swift", oldPath: nil, status: .modified, isBinary: false, oldMode: nil, newMode: nil,
                 hunks: [hunk(start: 1, count: 2), hunk(start: 20, count: 1)]),
        index: 0)

    @Test func selectionsMayCrossHunksButCommentsMayNot() {
        #expect(file.selection(from: 1, to: 2)?.line == 2)
        // Kept for asking the agent about both changes.
        let across = file.selection(from: 1, to: 4)
        #expect(across?.startLine == 1 && across?.line == 20)
        #expect(!file.crossesHunks(1...2))
        #expect(file.crossesHunks(1...4))
    }
}

private func hunk(start: UInt32, count: UInt32) -> DiffHunk {
    let lines = (start..<start + count).map { DiffLine(kind: .context, oldLine: $0, newLine: $0, text: "x") }
    return DiffHunk(header: "@@ -\(start),\(count) +\(start),\(count) @@", oldStart: start, oldCount: count,
                    newStart: start, newCount: count, lines: lines)
}

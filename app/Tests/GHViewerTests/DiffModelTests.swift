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

    @Test func fileTreePutsFoldersFirstAndJoinsChains() {
        let rows = fileTree(of: ["README.md", "src/app/models/a.swift", "src/app/models/b.swift", "src/main.swift"])
        #expect(rows == [.folder(path: "src/", name: "src", depth: 0),
                         .folder(path: "src/app/models/", name: "app/models", depth: 1),
                         .file(path: "src/app/models/a.swift", depth: 2),
                         .file(path: "src/app/models/b.swift", depth: 2),
                         .file(path: "src/main.swift", depth: 1),
                         .file(path: "README.md", depth: 0)])
    }

    @Test func foldersInTheFileTreeStartClosed() {
        let tree = fileTree(of: ["README.md", "src/app/a.swift", "src/main.swift"])
        #expect(DiffFileList.visible(tree, open: []).map(\.path) == ["src/", "README.md"])
        #expect(DiffFileList.visible(tree, open: ["src/"]).map(\.path) == ["src/", "src/app/", "src/main.swift", "README.md"])
        // An open folder inside a closed one stays hidden.
        #expect(DiffFileList.visible(tree, open: ["src/app/"]).map(\.path) == ["src/", "README.md"])
    }

    @Test func threadsGoUnderALineOnlyOnTheirOwnCommit() {
        let thread = { (id: String, line: UInt32?, commit: String?) in
            ReviewThread(id: id, path: "a.swift", line: line, startLine: nil, side: .right, startSide: nil,
                         isResolved: false, isOutdated: line == nil, commitOid: commit, comments: [])
        }
        let threads = [thread("here", 2, "shown"), thread("older", 2, "older"), thread("unknown", 2, nil),
                       thread("outdated", nil, "shown")]
        let annotations = DiffAnnotations(threads: threads, drafts: [], pending: [], headOid: "shown",
                                          pendingInline: true)
        #expect(annotations.threads[DiffAnchor(path: "a.swift", side: .right, line: 2)]?.map(\.id) == ["here"])
        #expect(annotations.notes["a.swift"]?.otherCommit.map(\.id) == ["older", "unknown"])
        #expect(annotations.notes["a.swift"]?.outdated.map(\.id) == ["outdated"])
        #expect(annotations.notes["a.swift"]?.summary == "1 outdated · 2 on another commit")
    }
}

private func hunk(start: UInt32, count: UInt32) -> DiffHunk {
    let lines = (start..<start + count).map { DiffLine(kind: .context, oldLine: $0, newLine: $0, text: "x") }
    return DiffHunk(header: "@@ -\(start),\(count) +\(start),\(count) @@", oldStart: start, oldCount: count,
                    newStart: start, newCount: count, lines: lines)
}

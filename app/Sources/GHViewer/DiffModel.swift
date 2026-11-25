import AppKit
import SwiftUI

/// A commentable diff line, named as GitHub's review API names it.
struct DiffAnchor: Hashable {
    let path: String
    let side: DiffSide
    let line: UInt32
}

/// One file of the diff, flattened into rows once per load so the lazy diff list stays cheap.
struct DiffFileModel: Identifiable {
    /// Above this many diff lines a file starts collapsed, behind "Show diff".
    static let largeLineCount = 1500

    let file: DiffFile
    /// Hunk headers and lines in diff order.
    let rows: [DiffRow]
    let additions: Int
    let deletions: Int
    /// Row position of every line, by the anchor a comment on it uses.
    let positions: [DiffAnchor: Int]
    /// Width of one line-number column, sized for the file's largest number.
    let numberWidth: CGFloat

    /// For a removed line and the added line that replaced it: the characters that differ, by row position.
    let emphasis: [Int: Range<Int>]

    var id: String { file.path }
    var lineCount: Int { positions.count }
    var isLarge: Bool { lineCount > Self.largeLineCount }
    /// Lockfiles, generated code and data dumps: files a reviewer skims at most, so they start collapsed.
    var isNoise: Bool {
        file.path.isGeneratedOrLockFile || (file.path.isDataFile && lineCount > Self.largeDataLineCount)
    }
    /// A small fixture is worth reading; a long data file isn't.
    static let largeDataLineCount = 200

    /// `index` is the file's place in the diff, which keeps its row ids apart from other files'.
    init(_ file: DiffFile, index: Int) {
        self.file = file
        var rows: [DiffRow] = []
        var positions: [DiffAnchor: Int] = [:]
        var additions = 0, deletions = 0
        var largest: UInt32 = 0
        for hunk in file.hunks {
            rows.append(DiffRow(id: index << 32 | rows.count, position: rows.count, kind: .hunk(hunk.header)))
            for line in hunk.lines {
                positions[line.anchor(in: file.path)] = rows.count
                rows.append(DiffRow(id: index << 32 | rows.count, position: rows.count, kind: .line(line)))
                switch line.kind {
                case .added: additions += 1
                case .removed: deletions += 1
                case .context: break
                }
                largest = max(largest, line.oldLine ?? 0, line.newLine ?? 0)
            }
        }
        // Pair each run of removed lines with the run of added lines right after it, line for line.
        var emphasis: [Int: Range<Int>] = [:]
        func text(_ row: Int, _ kind: LineKind) -> String? {
            guard row < rows.count, case .line(let line) = rows[row].kind, line.kind == kind else { return nil }
            return line.text
        }
        var row = 0
        while row < rows.count {
            let firstRemoved = row
            while text(row, .removed) != nil { row += 1 }
            let firstAdded = row
            while text(row, .added) != nil { row += 1 }
            if firstAdded > firstRemoved, row - firstAdded == firstAdded - firstRemoved {
                for offset in 0..<(firstAdded - firstRemoved) {
                    if let old = text(firstRemoved + offset, .removed), let new = text(firstAdded + offset, .added),
                       let changed = DiffHighlight.changedRanges(old: old, new: new) {
                        emphasis[firstRemoved + offset] = changed.old
                        emphasis[firstAdded + offset] = changed.new
                    }
                }
            }
            if row == firstRemoved { row += 1 }
        }
        self.emphasis = emphasis
        self.rows = rows
        self.positions = positions
        self.additions = additions
        self.deletions = deletions
        numberWidth = CGFloat(String(largest).count) * DiffStyle.digitWidth + 12
    }

    /// The selection spanning rows `start...end`, which must both be lines. It may cross hunks,
    /// which is fine for asking the agent; see `crossesHunks` for commenting.
    func selection(from start: Int, to end: Int) -> Selection? {
        guard case .line(let first) = rows[start].kind, case .line(let last) = rows[end].kind else { return nil }
        let from = first.anchor(in: id), to = last.anchor(in: id)
        return Selection(path: id, startLine: from.line, startSide: from.side, line: to.line, side: to.side)
    }

    /// GitHub takes a multi-line comment only within one hunk.
    func crossesHunks(_ rows: ClosedRange<Int>) -> Bool {
        self.rows[rows].contains(where: \.isHunk)
    }

    /// The rows `selection` covers, if it's in this file and its lines are in the diff.
    func rowRange(of selection: Selection?) -> ClosedRange<Int>? {
        guard let selection, selection.path == id,
              let start = positions[DiffAnchor(path: id, side: selection.startSide, line: selection.startLine)],
              let end = positions[DiffAnchor(path: id, side: selection.side, line: selection.line)],
              start <= end
        else { return nil }
        return start...end
    }
}

struct DiffRow: Identifiable, Equatable {
    enum Kind: Equatable {
        case hunk(String)
        case line(DiffLine)
    }

    /// Unique in the whole diff: a lazy stack mixes up rows of different sections that share an id.
    let id: Int
    /// Index in the file's rows, which is diff order.
    let position: Int
    let kind: Kind

    var isHunk: Bool {
        if case .hunk = kind { true } else { false }
    }
}

extension DiffLine {
    /// Removed lines are on the left with their old number; added and context lines on the right.
    func anchor(in path: String) -> DiffAnchor {
        kind == .removed
            ? DiffAnchor(path: path, side: .left, line: oldLine ?? 0)
            : DiffAnchor(path: path, side: .right, line: newLine ?? 0)
    }
}

/// Threads, drafts and pending github.com comments by the line they're shown under,
/// and per file what can't go under a line.
struct DiffAnnotations {
    var threads: [DiffAnchor: [ReviewThread]] = [:]
    var drafts: [DiffAnchor: [DraftComment]] = [:]
    var pending: [DiffAnchor: [PendingComment]] = [:]
    var notes: [String: DiffFileNotes] = [:]

    /// `headOid` is the commit the diff shows. A thread goes under its line only when GitHub placed
    /// it on that commit: drafts may pin the diff to an older one, a push may land while it loads,
    /// and a thread posted on an older commit stays there until the next push. `pendingInline` is
    /// false when the pending review is on another commit.
    init(threads: [ReviewThread], drafts: [DraftComment], pending: [PendingComment],
         headOid: String?, pendingInline: Bool) {
        for thread in threads {
            if let line = thread.line, thread.commitOid != nil, thread.commitOid == headOid {
                self.threads[DiffAnchor(path: thread.path, side: thread.side, line: line), default: []].append(thread)
            } else if thread.line == nil {
                notes[thread.path, default: DiffFileNotes()].outdated.append(thread)
            } else {
                notes[thread.path, default: DiffFileNotes()].otherCommit.append(thread)
            }
        }
        for draft in drafts {
            self.drafts[DiffAnchor(path: draft.path, side: draft.side, line: draft.line), default: []].append(draft)
        }
        for comment in pending {
            // A context line's comment may be on either side; GitHub numbers it like the right one's.
            if let line = comment.line, pendingInline {
                self.pending[DiffAnchor(path: comment.path, side: comment.side ?? .right, line: line), default: []]
                    .append(comment)
            } else {
                notes[comment.path, default: DiffFileNotes()].pending.append(comment)
            }
        }
    }

    /// Anchors whose threads and drafts show under `line`. A context line also takes
    /// comments made on its left side, as GitHub's split view allows.
    static func anchors(of line: DiffLine, in path: String) -> [DiffAnchor] {
        var anchors = [line.anchor(in: path)]
        if line.kind == .context, let old = line.oldLine {
            anchors.append(DiffAnchor(path: path, side: .left, line: old))
        }
        return anchors
    }
}

/// A file's comments that can't sit under a line of this diff, listed from its header.
struct DiffFileNotes {
    /// Threads on lines that are no longer in the PR.
    var outdated: [ReviewThread] = []
    /// Threads placed on another commit than the diff's: usually the latest push while drafts hold
    /// an older one, or an older commit a thread was posted on, until the next push moves it.
    var otherCommit: [ReviewThread] = []
    /// Your pending github.com comments on the whole file, or started on another commit.
    var pending: [PendingComment] = []

    var isEmpty: Bool { outdated.isEmpty && otherCommit.isEmpty && pending.isEmpty }

    /// "1 outdated · 2 on another commit · 1 pending"
    var summary: String {
        [(outdated.count, "outdated"), (otherCommit.count, "on another commit"), (pending.count, "pending")]
            .filter { $0.0 > 0 }
            .map { "\($0.0) \($0.1)" }
            .joined(separator: " · ")
    }
}

/// What the one open comment editor is for. Only one is open at a time, so ⌘↩ and Esc are unambiguous.
enum DiffEditorTarget: Equatable {
    case newComment(Selection)
    case draft(id: String)

    var newComment: Selection? {
        if case .newComment(let selection) = self { selection } else { nil }
    }
}

/// Asks the diff to scroll to a file; a fresh id repeats the scroll for the same file.
struct DiffScrollRequest: Equatable {
    let path: String
    let id = UUID()
}

/// A row of the file tree: a folder, or a changed file in it.
enum FileTreeRow: Equatable, Identifiable {
    case folder(path: String, name: String, depth: Int)
    case file(path: String, depth: Int)

    var id: String {
        switch self {
        case .folder(let path, _, _): "folder:" + path
        case .file(let path, _): path
        }
    }

    var path: String {
        switch self {
        case .folder(let path, _, _), .file(let path, _): path
        }
    }
}

/// The changed paths as a tree, flattened for a list. Folders come before files, and a chain of
/// folders with nothing else in them is one row ("app/models"), as on GitHub.
func fileTree(of paths: [String]) -> [FileTreeRow] {
    final class Folder {
        // Arrays searched linearly, to keep the diff's order; a dictionary if a PR ever has thousands of folders.
        var folders: [(name: String, folder: Folder)] = []
        var files: [String] = []
    }
    let root = Folder()
    for path in paths {
        var folder = root
        for name in path.split(separator: "/").dropLast().map(String.init) {
            if let existing = folder.folders.first(where: { $0.name == name }) {
                folder = existing.folder
            } else {
                let new = Folder()
                folder.folders.append((name, new))
                folder = new
            }
        }
        folder.files.append(path)
    }
    var rows: [FileTreeRow] = []
    func add(_ folder: Folder, prefix: String, depth: Int) {
        for entry in folder.folders {
            var name = entry.name
            var child = entry.folder
            while child.files.isEmpty, child.folders.count == 1 {
                name += "/" + child.folders[0].name
                child = child.folders[0].folder
            }
            let path = prefix + name + "/"
            rows.append(.folder(path: path, name: name, depth: depth))
            add(child, prefix: path, depth: depth + 1)
        }
        rows += folder.files.map { .file(path: $0, depth: depth) }
    }
    add(root, prefix: "", depth: 0)
    return rows
}

/// Diff metrics and tints. Colors come from the palette's roles, at row-background strength.
enum DiffStyle {
    static let font = Font.system(size: 12, design: .monospaced)
    static let digitWidth = ("0" as NSString)
        .size(withAttributes: [.font: NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)]).width
    static let rowOpacity = 0.09
    static let gutterOpacity = 0.15
    static let selectedOpacity = 0.26
    /// Hunk headers stay neutral, since every palette's hues belong to states.
    static let hunk = Color.primary.opacity(0.03)
}

extension String {
    var isGeneratedOrLockFile: Bool {
        let name = splitPath.name.lowercased()
        if ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "go.sum", "package.resolved", "bun.lockb"].contains(name)
            || [".lock", ".min.js", ".min.css", ".map", ".snap", ".pb.go", ".g.dart", ".generated.swift"].contains(where: name.hasSuffix) {
            return true
        }
        let path = "/" + lowercased()
        return ["/dist/", "/vendor/", "/node_modules/", "/__snapshots__/", "/generated/"].contains(where: path.contains)
    }

    var isDataFile: Bool {
        let name = lowercased()
        return [".json", ".jsonl", ".ndjson", ".csv", ".tsv", ".svg"].contains(where: name.hasSuffix)
    }

    /// "Sources/App/" and "File.swift", for paths drawn with the directory dimmed.
    var splitPath: (directory: String, name: String) {
        guard let slash = lastIndex(of: "/") else { return ("", self) }
        return (String(self[...slash]), String(self[index(after: slash)...]))
    }
}

/// "1 draft", "3 drafts".
func countLabel(_ count: Int, _ noun: String) -> String {
    "\(count) \(noun)\(count == 1 ? "" : "s")"
}

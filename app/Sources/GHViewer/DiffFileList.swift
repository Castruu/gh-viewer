import SwiftUI

/// The open PR's changed files as a folder tree, beside the diff. Clicking a file scrolls the diff to it.
struct DiffFileList: View {
    /// `UserDefaults` key: whether the tree shows. The toolbar and the Review menu switch it.
    static let showsKey = "showsFileTree"

    let review: PRReview
    /// The sidebar's filter: leave out the files already checked as viewed.
    @AppStorage("sidebarHidesViewedFiles") private var hidesViewed = false
    @State private var selection: String?
    /// The folders that are open, by path ending in "/". They start closed: a big PR is
    /// easier to take in as a few folders than as every file at once.
    @State private var open: Set<String> = []
    private static let indent: CGFloat = 12

    var body: some View {
        if let files = review.opened?.files {
            let draftCounts = Dictionary(grouping: review.drafts, by: \.path).mapValues(\.count)
            let viewed = review.viewedPaths
            let byPath = Dictionary(files.map { ($0.path, $0) }) { first, _ in first }
            // The tree is rebuilt on each redraw, which is nothing for hundreds of files; cache it if a PR with thousands drags.
            let paths = review.shown(files, path: \.path).map(\.path)
            let tree = fileTree(of: paths.filter { !hidesViewed || !viewed.contains($0) })
            let rows = Self.visible(tree, open: open)
            List(rows, selection: Binding(get: { selection }, set: { path in
                selection = path
                if let path { review.scrollRequest = DiffScrollRequest(path: path) }
            })) { row in
                switch row {
                case .folder(let path, let name, let depth):
                    Button {
                        open.formSymmetricDifference([path])
                    } label: {
                        HStack(spacing: 4) {
                            Image(systemName: "chevron.right")
                                .font(.caption2.weight(.semibold))
                                .rotationEffect(.degrees(open.contains(path) ? 90 : 0))
                                .frame(width: 10)
                            Label(name, systemImage: "folder")
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .padding(.leading, CGFloat(depth) * Self.indent)
                    .selectionDisabled()
                    .listRowSeparator(.hidden)
                    .help(open.contains(path) ? "Hide the files in \(path)" : "Show the files in \(path)")
                case .file(let path, let depth):
                    if let file = byPath[path] {
                        DiffFileRow(file: file, draftCount: draftCounts[path] ?? 0, isViewed: viewed.contains(path))
                            .padding(.leading, CGFloat(depth) * Self.indent)
                            .listRowSeparator(.hidden)
                    }
                }
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 24)
            .overlay {
                if rows.isEmpty {
                    ContentUnavailableView("All Files Viewed", systemImage: "checkmark.circle",
                                           description: Text("The filter hides viewed files."))
                }
            }
        } else {
            ContentUnavailableView("No Files Yet", systemImage: "doc.on.doc",
                                   description: Text("They show once the pull request has loaded."))
        }
    }
}

extension DiffFileList {
    /// The rows to draw: those whose folders, all the way up, are open.
    static func visible(_ tree: [FileTreeRow], open: Set<String>) -> [FileTreeRow] {
        let folders = tree.compactMap { row in
            if case .folder(let path, _, _) = row { path } else { nil }
        }
        return tree.filter { row in
            folders.allSatisfy { !row.path.hasPrefix($0) || row.path == $0 || open.contains($0) }
        }
    }
}

private struct DiffFileRow: View {
    let file: DiffFile
    let draftCount: Int
    let isViewed: Bool

    var body: some View {
        HStack(spacing: 6) {
            DiffStatusMark(status: file.status)
            VStack(alignment: .leading, spacing: 1) {
                // The folder rows above say where it is.
                Text(verbatim: file.path.splitPath.name)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let oldPath = file.oldPath {
                    Text(verbatim: "\(oldPath) →")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            Spacer(minLength: 4)
            if draftCount > 0 {
                Label("\(draftCount)", systemImage: "text.bubble")
                    .labelStyle(.titleAndIcon)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .help(countLabel(draftCount, "draft"))
            }
            if isViewed {
                Image(systemName: "checkmark")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .help("Viewed on GitHub")
            }
        }
        .opacity(isViewed ? 0.6 : 1)
        .help(file.oldPath.map { "\($0) → \(file.path)" } ?? file.path)
    }
}

/// A file's path with the directory dimmed and the name emphasized. Renames can show "old → new".
struct DiffPathText: View {
    let file: DiffFile
    let renameArrow: Bool

    var body: some View {
        let (directory, name) = file.path.splitPath
        // Explicit colors: hierarchical styles resolve unpredictably inside concatenated text in lists.
        var text = Text(verbatim: directory).foregroundStyle(Color.secondary)
            + Text(verbatim: name).fontWeight(.semibold).foregroundStyle(Color.primary)
        if renameArrow, let oldPath = file.oldPath {
            text = Text(verbatim: "\(oldPath) → ").foregroundStyle(Color.secondary) + text
        }
        return text
            .lineLimit(1)
            // Cut the start, so the file name stays visible.
            .truncationMode(.head)
    }
}

/// A, M, D or R in the status's color.
struct DiffStatusMark: View {
    let status: FileStatus
    @Environment(\.palette) private var palette

    var body: some View {
        Text(letter)
            .font(.caption.monospaced().weight(.bold))
            .foregroundStyle(color)
            .frame(width: 16, height: 16)
            .background(color.opacity(0.15), in: RoundedRectangle(cornerRadius: 3))
            .help(name)
    }

    private var letter: String {
        switch status {
        case .added: "A"
        case .modified: "M"
        case .deleted: "D"
        case .renamed: "R"
        }
    }

    private var name: String {
        switch status {
        case .added: "Added"
        case .modified: "Modified"
        case .deleted: "Deleted"
        case .renamed: "Renamed"
        }
    }

    private var color: Color {
        switch status {
        case .added: palette.added
        case .modified: palette.pending
        case .deleted: palette.removed
        case .renamed: palette.accent
        }
    }
}

/// "+12 −3" in the palette's diff colors.
struct DiffLineCounts: View {
    let additions: Int
    let deletions: Int
    @Environment(\.palette) private var palette

    var body: some View {
        (Text(verbatim: "+\(additions)").foregroundStyle(palette.added)
            + Text(verbatim: " −\(deletions)").foregroundStyle(palette.removed))
            .monospacedDigit()
            .lineLimit(1)
            .fixedSize()
            .help("\(countLabel(additions, "addition")), \(countLabel(deletions, "deletion"))")
    }
}

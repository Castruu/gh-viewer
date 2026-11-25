import SwiftUI

/// The changed files beside the diff. Clicking one scrolls the diff to it.
struct DiffFileList: View {
    let review: PRReview
    let files: [DiffFileModel]
    let select: (String) -> Void
    let setViewed: (String, Bool) -> Void
    @State private var selection: String?

    var body: some View {
        let draftCounts = Dictionary(grouping: review.drafts, by: \.path).mapValues(\.count)
        let viewed = review.viewedPaths
        List(files, selection: Binding(get: { selection }, set: { path in
            selection = path
            if let path { select(path) }
        })) { file in
            DiffFileRow(file: file, draftCount: draftCounts[file.id] ?? 0, isViewed: viewed.contains(file.id),
                        canMarkViewed: !review.hasNewerCommits) {
                setViewed(file.id, $0)
            }
        }
        .listStyle(.plain)
    }
}

private struct DiffFileRow: View {
    let file: DiffFileModel
    let draftCount: Int
    let isViewed: Bool
    let canMarkViewed: Bool
    let setViewed: (Bool) -> Void

    var body: some View {
        HStack(spacing: 6) {
            DiffStatusMark(status: file.file.status)
            VStack(alignment: .leading, spacing: 1) {
                DiffPathText(file: file.file, renameArrow: false)
                if let oldPath = file.file.oldPath {
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
            DiffLineCounts(additions: file.additions, deletions: file.deletions)
                .font(.caption)
            Toggle("Viewed", isOn: Binding(get: { isViewed }, set: setViewed))
                .toggleStyle(.checkbox)
                .labelsHidden()
                .disabled(!canMarkViewed)
                .help(!canMarkViewed ? PRReview.viewedPinnedHelp
                      : isViewed ? "Viewed on GitHub. Uncheck to mark as not viewed" : "Mark as viewed on GitHub")
        }
        .opacity(isViewed ? 0.6 : 1)
        .help(file.file.oldPath.map { "\($0) → \(file.id)" } ?? file.id)
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

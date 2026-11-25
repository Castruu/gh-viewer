import AppKit
import SwiftUI

/// The agent's markdown as paragraphs, fenced code blocks and tables. `Text` only renders
/// inline markdown, so the blocks are split here, each as its own view: while a reply
/// streams, only its last block changes and gets parsed again.
struct ChatMarkdown: View {
    let markdown: String

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(ChatMarkdownBlock.split(markdown).enumerated()), id: \.offset) { _, block in
                switch block {
                case .text(let text):
                    ChatMarkdownText(source: text)
                case .code(let language, let code):
                    ChatCodeBlock(language: language, code: code)
                case .table(let rows):
                    ChatTable(rows: rows)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// Inline markdown with line breaks kept; plain text if it doesn't parse.
    static func inline(_ source: String) -> AttributedString {
        (try? AttributedString(markdown: source, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? AttributedString(source)
    }
}

enum ChatMarkdownBlock: Equatable {
    case text(String)
    case code(language: String, code: String)
    /// Cells by row; the first row is the header.
    case table([[String]])

    /// Splits at fenced code blocks, tables and blank lines. An unclosed fence, as while
    /// streaming, runs to the end. A table starts once its `|---|` row has arrived.
    static func split(_ markdown: String) -> [ChatMarkdownBlock] {
        let all = markdown.split(separator: "\n", omittingEmptySubsequences: false)
        var blocks: [ChatMarkdownBlock] = []
        var lines: [Substring] = []
        var fence: (marker: String, language: String)?
        var table: [[String]]?

        func flushText() {
            if !lines.isEmpty {
                blocks.append(.text(lines.map(styled).joined(separator: "\n")))
                lines = []
            }
        }

        func flushTable() {
            if let rows = table {
                blocks.append(.table(rows))
                table = nil
            }
        }

        var index = all.startIndex
        while index < all.endIndex {
            let line = all[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            index += 1
            if let open = fence {
                if trimmed.count >= open.marker.count && trimmed.allSatisfy({ $0 == open.marker.first }) {
                    blocks.append(.code(language: open.language, code: lines.joined(separator: "\n")))
                    lines = []
                    fence = nil
                } else {
                    lines.append(line)
                }
            } else if table != nil && trimmed.hasPrefix("|") {
                table?.append(cells(trimmed))
            } else if trimmed.hasPrefix("|") && index < all.endIndex && isDelimiterRow(all[index]) {
                flushText()
                flushTable()
                table = [cells(trimmed)]
                index += 1
            } else {
                flushTable()
                if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                    flushText()
                    let marker = String(trimmed.prefix { $0 == trimmed.first })
                    fence = (marker, trimmed.dropFirst(marker.count).trimmingCharacters(in: .whitespaces))
                } else if trimmed.isEmpty {
                    flushText()
                } else {
                    lines.append(line)
                }
            }
        }
        flushTable()
        if let fence {
            blocks.append(.code(language: fence.language, code: lines.joined(separator: "\n")))
        } else {
            flushText()
        }
        return blocks
    }

    /// "| a | `b` |" → ["a", "`b`"].
    /// shortcut: an escaped pipe (`\|`) still splits a cell; handle it if agents start writing them.
    private static func cells(_ row: String) -> [String] {
        var row = Substring(row)
        if row.hasPrefix("|") {
            row = row.dropFirst()
        }
        if row.hasSuffix("|") {
            row = row.dropLast()
        }
        return row.split(separator: "|", omittingEmptySubsequences: false)
            .map { $0.trimmingCharacters(in: .whitespaces) }
    }

    /// The row under a table's header, e.g. "|---|:---:|".
    private static func isDelimiterRow(_ line: Substring) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        return trimmed.hasPrefix("|") && trimmed.contains("-") && trimmed.allSatisfy { "|-: ".contains($0) }
    }

    /// Headings become bold lines and list markers bullets, since inline-only parsing shows them verbatim.
    private static func styled(_ line: Substring) -> String {
        let indent = line.prefix { $0 == " " }
        let rest = line.dropFirst(indent.count)
        if rest.hasPrefix("#") {
            let title = rest.drop { $0 == "#" }
            if title.hasPrefix(" ") {
                return "\(indent)**\(title.dropFirst())**"
            }
        }
        if rest.hasPrefix("- ") || rest.hasPrefix("* ") {
            return "\(indent)• \(rest.dropFirst(2))"
        }
        return String(line)
    }
}

/// A markdown table with a bold header. Cells wrap to fit the pane.
private struct ChatTable: View {
    let rows: [[String]]

    var body: some View {
        let columns = rows.map(\.count).max() ?? 0
        Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
            ForEach(Array(rows.enumerated()), id: \.offset) { index, row in
                GridRow {
                    ForEach(0..<columns, id: \.self) { column in
                        Text(ChatMarkdown.inline(column < row.count ? row[column] : ""))
                            .fontWeight(index == 0 ? .semibold : nil)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                if index == 0 {
                    Divider()
                }
            }
        }
        .textSelection(.enabled)
        .padding(8)
        .background(.quaternary.opacity(0.35), in: .rect(cornerRadius: 6))
    }
}

private struct ChatMarkdownText: View {
    let source: String

    var body: some View {
        Text(ChatMarkdown.inline(source))
            .textSelection(.enabled)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// A fenced code block: monospaced, scrolling sideways instead of wrapping, with a copy button.
private struct ChatCodeBlock: View {
    let language: String
    let code: String

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(language)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(code, forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc")
                }
                .buttonStyle(.borderless)
                .help("Copy code")
            }
            .padding(.horizontal, 8)
            .padding(.top, 4)
            ScrollView(.horizontal) {
                Text(code)
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
                    .fixedSize()
                    .padding([.horizontal, .bottom], 8)
                    .padding(.top, 2)
            }
        }
        .background(.quaternary.opacity(0.6), in: .rect(cornerRadius: 6))
    }
}

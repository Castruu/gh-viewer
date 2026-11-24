import AppKit
import SwiftUI

/// The agent's markdown as paragraphs and fenced code blocks. `Text` only renders inline
/// markdown, so the blocks are split here, each as its own view: while a reply streams,
/// only its last block changes and gets parsed again.
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

    /// Splits at fenced code blocks and blank lines. An unclosed fence, as while
    /// streaming, runs to the end.
    static func split(_ markdown: String) -> [ChatMarkdownBlock] {
        var blocks: [ChatMarkdownBlock] = []
        var lines: [Substring] = []
        var fence: (marker: String, language: String)?

        func flushText() {
            if !lines.isEmpty {
                blocks.append(.text(lines.map(styled).joined(separator: "\n")))
                lines = []
            }
        }

        for line in markdown.split(separator: "\n", omittingEmptySubsequences: false) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if let open = fence {
                if trimmed.count >= open.marker.count && trimmed.allSatisfy({ $0 == open.marker.first }) {
                    blocks.append(.code(language: open.language, code: lines.joined(separator: "\n")))
                    lines = []
                    fence = nil
                } else {
                    lines.append(line)
                }
            } else if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                flushText()
                let marker = String(trimmed.prefix { $0 == trimmed.first })
                fence = (marker, trimmed.dropFirst(marker.count).trimmingCharacters(in: .whitespaces))
            } else if trimmed.isEmpty {
                flushText()
            } else {
                lines.append(line)
            }
        }
        if let fence {
            blocks.append(.code(language: fence.language, code: lines.joined(separator: "\n")))
        } else {
            flushText()
        }
        return blocks
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

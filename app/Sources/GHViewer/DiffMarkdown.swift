import SwiftUI

/// A review comment's markdown: inline styles and links, with fenced code blocks drawn monospaced.
struct DiffMarkdown: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(Self.blocks(of: text).enumerated()), id: \.offset) { _, block in
                if block.isCode {
                    Text(verbatim: block.text)
                        .font(.system(.callout, design: .monospaced))
                        .padding(6)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(.quaternary.opacity(0.6), in: RoundedRectangle(cornerRadius: 4))
                } else {
                    Text(Self.inline(block.text))
                }
            }
        }
        .textSelection(.enabled)
    }

    static func inline(_ text: String) -> AttributedString {
        let text = text.replacing(htmlTag, with: "")
        return (try? AttributedString(markdown: text, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? AttributedString(text)
    }

    /// HTML that bots and templates put in comments, which GitHub renders and we can't.
    /// shortcut: drops the tags and keeps their text (images vanish); render HTML if it matters.
    private static let htmlTag =
        #/<\/?(?i:a|img|details|summary|br|p|div|span|sub|sup|b|i|em|strong|picture|source|table|thead|tbody|tr|td|th|ul|ol|li|h[1-6]|hr|blockquote)\b[^>\n]*>/#

    /// Splits at ``` fences. An unclosed fence runs to the end, as on GitHub.
    static func blocks(of text: String) -> [(isCode: Bool, text: String)] {
        var blocks: [(isCode: Bool, text: String)] = []
        var lines: [Substring] = []
        var inCode = false
        func flush() {
            let joined = lines.joined(separator: "\n")
            if inCode || !joined.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                blocks.append((inCode, joined.trimmingCharacters(in: .newlines)))
            }
            lines = []
        }
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            if line.trimmingCharacters(in: .whitespaces).hasPrefix("```") {
                flush()
                inCode.toggle()
            } else {
                lines.append(line)
            }
        }
        flush()
        return blocks
    }
}

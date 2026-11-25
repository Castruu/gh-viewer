import SwiftUI

/// Colors for code in the diff: comments, strings, numbers and common keywords, plus the part
/// of an edited line that changed. One scanner for every language, told apart only by how a
/// comment starts, so it's cheap enough to run for each visible line. It reads one line at a
/// time and doesn't know about strings or comments that span lines.
enum DiffHighlight {
    enum Kind: Equatable {
        case comment, string, number, keyword
    }

    /// `range` counts characters of the line.
    struct Token: Equatable {
        let range: Range<Int>
        let kind: Kind
    }

    struct Language: Equatable {
        /// What starts a comment that runs to the end of the line, if anything.
        let comment: String?
        /// False where an apostrophe is something else, like a Rust lifetime.
        let singleQuotes: Bool
    }

    /// `nil` for prose and for files we can't place: those stay plain.
    static func language(of path: String) -> Language? {
        let name = path.splitPath.name.lowercased()
        let ext = name.split(separator: ".").last.map(String.init) ?? name
        if ["js", "jsx", "ts", "tsx", "mjs", "cjs", "php", "dart", "groovy", "gradle", "scss", "jsonc"].contains(ext) {
            return Language(comment: "//", singleQuotes: true)
        }
        if ["swift", "rs", "go", "java", "kt", "kts", "c", "h", "cc", "cpp", "hpp", "cs", "scala", "m", "mm", "zig", "proto"].contains(ext) {
            return Language(comment: "//", singleQuotes: false)
        }
        if ["py", "rb", "sh", "bash", "zsh", "fish", "yml", "yaml", "toml", "pl", "r", "tf", "nix", "ex", "exs",
            "ini", "cfg", "conf", "dockerfile", "makefile"].contains(ext) {
            return Language(comment: "#", singleQuotes: true)
        }
        if ["sql", "lua"].contains(ext) {
            return Language(comment: "--", singleQuotes: true)
        }
        if ["json", "css"].contains(ext) {
            return Language(comment: nil, singleQuotes: ext == "css")
        }
        return nil
    }

    private static let keywords: Set<String> = [
        "func", "fn", "def", "let", "var", "const", "if", "else", "elif", "for", "while", "return", "import", "from",
        "class", "struct", "enum", "protocol", "interface", "impl", "trait", "pub", "use", "match", "switch", "case",
        "default", "break", "continue", "async", "await", "try", "catch", "except", "finally", "throw", "throws",
        "raise", "guard", "in", "as", "is", "nil", "null", "None", "true", "false", "True", "False", "self", "static",
        "private", "public", "internal", "final", "extension", "package", "defer", "new", "export", "extends",
        "implements", "void", "lambda", "with", "yield", "mod", "where", "mut", "not", "and", "or", "type",
    ]

    static func tokens(in text: String, language: Language) -> [Token] {
        let chars = Array(text)
        let comment = language.comment.map(Array.init)
        var tokens: [Token] = []
        var index = 0
        func isWord(_ char: Character) -> Bool { char.isLetter || char.isNumber || char == "_" }
        while index < chars.count {
            let char = chars[index]
            if let comment, chars[index...].starts(with: comment),
               // "#" also sits inside words and after "$" in shell; a comment starts a line or follows a space.
               comment != ["#"] || index == 0 || chars[index - 1].isWhitespace {
                tokens.append(Token(range: index..<chars.count, kind: .comment))
                break
            }
            if char == "\"" || char == "`" || (char == "'" && language.singleQuotes) {
                var end = index + 1
                while end < chars.count, chars[end] != char {
                    end += chars[end] == "\\" ? 2 : 1
                }
                end = min(end + 1, chars.count)
                tokens.append(Token(range: index..<end, kind: .string))
                index = end
            } else if char.isNumber, index == 0 || !isWord(chars[index - 1]) {
                var end = index
                while end < chars.count, isWord(chars[end]) || chars[end] == "." { end += 1 }
                tokens.append(Token(range: index..<end, kind: .number))
                index = end
            } else if isWord(char) {
                var end = index
                while end < chars.count, isWord(chars[end]) { end += 1 }
                if keywords.contains(String(chars[index..<end])) {
                    tokens.append(Token(range: index..<end, kind: .keyword))
                }
                index = end
            } else {
                index += 1
            }
        }
        return tokens
    }

    /// What differs between a removed line and the added line that replaced it: what's left
    /// after their common start and end, counted in characters. `nil` when the two share too
    /// little for that to mean anything, which is a rewrite rather than an edit.
    static func changedRanges(old: String, new: String) -> (old: Range<Int>, new: Range<Int>)? {
        let a = Array(old), b = Array(new)
        let shortest = min(a.count, b.count)
        var prefix = 0
        while prefix < shortest, a[prefix] == b[prefix] { prefix += 1 }
        var suffix = 0
        while suffix < shortest - prefix, a[a.count - 1 - suffix] == b[b.count - 1 - suffix] { suffix += 1 }
        // Indentation is shared by any two lines, so it doesn't count.
        let shared = (a[..<prefix] + a[(a.count - suffix)...]).filter { !$0.isWhitespace }.count
        let longest = max(a.filter { !$0.isWhitespace }.count, b.filter { !$0.isWhitespace }.count)
        guard shared >= 3, shared * 3 >= longest else { return nil }
        return (prefix..<(a.count - suffix), prefix..<(b.count - suffix))
    }

    /// The line as shown: its marker, its text colored by `tokens`, and `emphasis` on a stronger tint.
    static func attributed(marker: String, text: String, path: String, emphasis: Range<Int>?,
                           palette: Palette, tint: Color) -> AttributedString {
        var result = AttributedString(marker)
        let chars = Array(text)
        // A line this long is minified or data; coloring it only costs time.
        guard chars.count <= 1000 else { return result + AttributedString(text) }
        var kinds = [Kind?](repeating: nil, count: chars.count)
        if let language = language(of: path) {
            for token in tokens(in: text, language: language) {
                for index in token.range { kinds[index] = token.kind }
            }
        }
        var start = 0
        while start < chars.count {
            let isEmphasized = emphasis?.contains(start) ?? false
            var end = start + 1
            while end < chars.count, kinds[end] == kinds[start], (emphasis?.contains(end) ?? false) == isEmphasized { end += 1 }
            var piece = AttributedString(String(chars[start..<end]))
            switch kinds[start] {
            case .comment: piece.foregroundColor = .secondary
            case .string: piece.foregroundColor = palette.agent
            case .number: piece.foregroundColor = palette.pending
            case .keyword: piece.foregroundColor = palette.accent
            case nil: break
            }
            if isEmphasized { piece.backgroundColor = tint.opacity(0.35) }
            result += piece
            start = end
        }
        return result
    }
}

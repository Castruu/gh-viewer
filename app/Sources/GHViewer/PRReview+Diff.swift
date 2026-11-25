import Foundation

extension PRReview {
    /// Changes whenever the diff does: a new head, or a new merge base after the base branch moved.
    var diffKey: String? {
        opened.map { "\($0.checkout.mergeBaseOid)..\($0.checkout.headOid)" }
    }

    /// Paths checked as viewed on GitHub. A file that changed since ("dismissed") isn't.
    var viewedPaths: Set<String> {
        Set(opened?.detail.files.filter { $0.viewed == .viewed }.map(\.path) ?? [])
    }

    /// GitHub marks a path as viewed at the PR's latest version, so only while that's what the diff
    /// shows: the displayed head, GitHub's head and the last fetched head agree, and no reload is replacing them.
    var canMarkViewed: Bool {
        !isReloading && opened?.showsLatestPush == true
    }

    /// Why "Viewed" is off when `canMarkViewed` isn't.
    static let viewedUnavailableHelp = "Unavailable while the diff isn't the PR's latest push or is reloading: GitHub would also mark changes you haven't seen as viewed"

    /// The commit the diff shows, which is the drafts' commit while they hold it.
    var shortHeadOid: String {
        String(opened?.checkout.headOid.prefix(7) ?? "")
    }

    /// How many of `comments` aren't one of `drafts`, paired one to one as the core does: path,
    /// original range, text, and side where a comment's is known.
    nonisolated static func startedOnGitHub(_ comments: [PendingComment], drafts: [DraftComment]) -> Int {
        let trimmed = { (text: String) in text.trimmingCharacters(in: .whitespacesAndNewlines) }
        // A single-line range is sent without a start line, whichever way it was stored.
        let start = { (start: UInt32?, line: UInt32?) in start == line ? nil : start }
        let same = { (draft: DraftComment, comment: PendingComment) in
            comment.path == draft.path && comment.originalLine == draft.line
                && start(comment.originalStartLine, comment.originalLine) == start(draft.startLine, draft.line)
                && trimmed(comment.body) == trimmed(draft.body)
                && (comment.side == .right || draft.side == .left)
        }
        var used = Array(repeating: false, count: comments.count)
        var found = Array(repeating: false, count: drafts.count)
        // Known-left comments first, so a draft that could take either leaves them be.
        for left in [true, false] {
            for (index, draft) in drafts.enumerated() where !found[index] {
                if let match = comments.indices.first(where: {
                    !used[$0] && (comments[$0].side == .left) == left && same(draft, comments[$0])
                }) {
                    used[match] = true
                    found[index] = true
                }
            }
        }
        return used.filter { !$0 }.count
    }
}

extension OpenedReview {
    /// The diff is on GitHub's head, which is also the latest one fetched.
    var showsLatestPush: Bool {
        checkout.headOid == detail.headOid && checkout.headOid == checkout.latestHeadOid
    }
}

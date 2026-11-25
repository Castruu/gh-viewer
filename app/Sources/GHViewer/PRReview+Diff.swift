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

    /// Why "Viewed" is off while `hasNewerCommits`: GitHub marks a path, so it would cover changes not shown here.
    static let viewedPinnedHelp = "Unavailable while the diff is on an older commit: GitHub would also mark the newer changes to this file as viewed"

    /// The commit the diff shows, which is the drafts' commit while they hold it.
    var shortHeadOid: String {
        String(opened?.checkout.headOid.prefix(7) ?? "")
    }

    /// Comments in your pending review on github.com, which submitting sends too.
    var pendingCommentCount: Int? {
        opened?.detail.pendingReview?.comments.count
    }
}

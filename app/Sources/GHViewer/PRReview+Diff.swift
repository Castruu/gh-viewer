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

    /// Comments in your pending review on github.com, which submitting sends too.
    var pendingCommentCount: Int? {
        opened?.detail.pendingReview?.comments.count
    }
}

extension OpenedReview {
    /// The diff is on GitHub's head, which is also the latest one fetched.
    var showsLatestPush: Bool {
        checkout.headOid == detail.headOid && checkout.headOid == checkout.latestHeadOid
    }
}

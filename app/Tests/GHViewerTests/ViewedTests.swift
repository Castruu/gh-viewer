import Testing
@testable import GHViewer

struct ViewedTests {
    @Test func viewedOnlyOnTheLatestPush() {
        #expect(opened(shown: "b", onGitHub: "b", fetched: "b").showsLatestPush)
        // Drafts pin the diff to an older commit.
        #expect(!opened(shown: "a", onGitHub: "b", fetched: "b").showsLatestPush)
        // GitHub moved on after the last fetch.
        #expect(!opened(shown: "b", onGitHub: "c", fetched: "b").showsLatestPush)
    }

    private func opened(shown: String, onGitHub: String, fetched: String) -> OpenedReview {
        let detail = PullRequestDetail(id: "PR_1", repo: "o/r", number: 7, title: "", body: "", url: "", author: nil,
                                       viewerDidAuthor: false, isDraft: false, baseRefName: "main", headRefName: "h",
                                       headOid: onGitHub, files: [], threads: [], pendingReview: nil,
                                       failedChecks: [], lastReviewedOid: nil)
        let checkout = Checkout(repo: "o/r", number: 7, worktreePath: "/tmp/wt", headOid: shown, latestHeadOid: fetched,
                                baseOid: "c", mergeBaseOid: "m")
        return OpenedReview(detail: detail, checkout: checkout, files: [], changedSinceReview: nil)
    }
}

import Testing
@testable import GHViewer

struct PendingCommentTests {
    @Test func draftsAlreadyInThePendingReviewArentCountedAsYours() {
        let draft = DraftComment(id: "1", path: "src/a.rs", line: 12, side: .right, startLine: nil, startSide: nil,
                                 body: "Nit", author: .you, commitOid: "abc")
        let comments = [
            // Put there by an earlier try.
            PendingComment(path: "src/a.rs", line: 12, startLine: nil, side: .right, body: "Nit\n"),
            // Started on github.com.
            PendingComment(path: "src/a.rs", line: 13, startLine: nil, side: .right, body: "Nit"),
            PendingComment(path: "README.md", line: nil, startLine: nil, side: .right, body: "Whole file"),
        ]
        #expect(PRReview.startedOnGitHub(comments, drafts: [draft]) == 2)
        #expect(PRReview.startedOnGitHub(comments, drafts: []) == 3)
    }
}

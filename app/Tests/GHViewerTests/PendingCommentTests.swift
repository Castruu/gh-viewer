import Testing
@testable import GHViewer

struct PendingCommentTests {
    @Test func draftsAlreadyInThePendingReviewArentCountedAsYours() {
        let draft = DraftComment(id: "1", path: "src/a.rs", line: 12, side: .right, startLine: nil, startSide: nil,
                                 body: "Nit", author: .you, commitOid: "abc")
        let comments = [
            // Put there by an earlier try.
            PendingComment(path: "src/a.rs", line: 12, startLine: nil, originalLine: 12, originalStartLine: nil,
                           side: .right, body: "Nit\n"),
            // Started on github.com.
            PendingComment(path: "src/a.rs", line: 13, startLine: nil, originalLine: 13, originalStartLine: nil,
                           side: .right, body: "Nit"),
            PendingComment(path: "README.md", line: nil, startLine: nil, originalLine: nil, originalStartLine: nil,
                           side: .right, body: "Whole file"),
        ]
        #expect(PRReview.startedOnGitHub(comments, drafts: [draft]) == 2)
        #expect(PRReview.startedOnGitHub(comments, drafts: []) == 3)
    }

    @Test func pairsDraftsAndCommentsOneToOneOnTheirOriginalRange() {
        let draft = DraftComment(id: "1", path: "src/a.rs", line: 12, side: .right, startLine: nil, startSide: nil,
                                 body: "Nit", author: .you, commitOid: "abc")
        let mine = PendingComment(path: "src/a.rs", line: 20, startLine: nil, originalLine: 12, originalStartLine: nil,
                                  side: .right, body: "Nit")
        // A later push moved it to line 20; it's still the draft's.
        #expect(PRReview.startedOnGitHub([mine], drafts: [draft]) == 0)
        // The same text twice: only one is the draft's.
        #expect(PRReview.startedOnGitHub([mine, mine], drafts: [draft]) == 1)
        // A range on lines 8–12 isn't a draft on line 12.
        let range = PendingComment(path: "src/a.rs", line: 12, startLine: 8, originalLine: 12, originalStartLine: 8,
                                   side: .right, body: "Nit")
        #expect(PRReview.startedOnGitHub([range], drafts: [draft]) == 1)
        // GitHub left out the original range: the current one stands in.
        let unoriginal = PendingComment(path: "src/a.rs", line: 12, startLine: nil, originalLine: nil,
                                        originalStartLine: nil, side: .right, body: "Nit")
        #expect(PRReview.startedOnGitHub([unoriginal], drafts: [draft]) == 0)
    }

    @Test func anAddedLinesCommentIsntTheRemovedLinesDraft() {
        let right = DraftComment(id: "r", path: "src/a.rs", line: 12, side: .right, startLine: nil, startSide: nil,
                                 body: "Same", author: .you, commitOid: "abc")
        let left = DraftComment(id: "l", path: "src/a.rs", line: 12, side: .left, startLine: nil, startSide: nil,
                                body: "Same", author: .you, commitOid: "abc")
        let added = PendingComment(path: "src/a.rs", line: 12, startLine: nil, originalLine: 12, originalStartLine: nil,
                                   side: .right, body: "Same")
        let context = PendingComment(path: "src/a.rs", line: 12, startLine: nil, originalLine: 12,
                                     originalStartLine: nil, side: nil, body: "Same")
        #expect(PRReview.startedOnGitHub([added], drafts: [left]) == 1)
        #expect(PRReview.startedOnGitHub([added], drafts: [left, right]) == 0)
        // A context line's comment may be either draft's, after the known ones are paired.
        #expect(PRReview.startedOnGitHub([context, added], drafts: [left, right]) == 0)
    }
}

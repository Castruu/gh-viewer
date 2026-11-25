import Testing
@testable import GHViewer

struct ListTests {
    @Test func rowsStartWithWhatThePRDoes() {
        #expect("feat(skills): add a pr skill".withoutCommitPrefix == "Add a pr skill")
        #expect("fix!: crash on launch".withoutCommitPrefix == "Crash on launch")
        #expect("Join MARS on the platform".withoutCommitPrefix == "Join MARS on the platform")
        // Not a commit type, and nothing left after one.
        #expect("Note: read this".withoutCommitPrefix == "Note: read this")
        #expect("docs: ".withoutCommitPrefix == "docs: ")
    }

    @Test func aSubmittedReviewMovesOnToTheNextWaitingPR() {
        #expect(AppModel.next(after: "b", in: ["a", "b", "c"]) == "c")
        #expect(AppModel.next(after: "c", in: ["a", "b", "c"]) == "a")
        // A review of a PR that wasn't waiting on you, and the last one waiting.
        #expect(AppModel.next(after: "x", in: ["a", "b"]) == nil)
        #expect(AppModel.next(after: "a", in: ["a"]) == nil)
    }
}

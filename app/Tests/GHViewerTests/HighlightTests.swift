import Foundation
import Testing
@testable import GHViewer

struct HighlightTests {
    @Test func anEditedLineMarksOnlyWhatChanged() {
        let changed = DiffHighlight.changedRanges(old: "    let timeout = 30", new: "    let timeout = 60")
        #expect(changed?.old == 18..<19 && changed?.new == 18..<19)
        // An insertion has nothing to mark on the old side.
        let inserted = DiffHighlight.changedRanges(old: "retry(request)", new: "retry(request, attempts)")
        #expect(inserted?.old == 13..<13 && inserted?.new == 13..<23)
        // Two lines that only share their indentation are a rewrite.
        #expect(DiffHighlight.changedRanges(old: "        return nil", new: "        throw error") == nil)
    }

    @Test func codeGetsCommentsStringsNumbersAndKeywords() {
        let swift = DiffHighlight.language(of: "Sources/App/Retry.swift")!
        #expect(DiffHighlight.tokens(in: #"let url = "https://a.b" // 3 tries"#, language: swift) == [
            .init(range: 0..<3, kind: .keyword), .init(range: 10..<23, kind: .string), .init(range: 24..<34, kind: .comment),
        ])
        let shell = DiffHighlight.language(of: "scripts/build.sh")!
        // "$#" isn't a comment, and a quote that never closes runs to the end of the line.
        #expect(DiffHighlight.tokens(in: "echo $# 'done", language: shell) == [.init(range: 8..<13, kind: .string)])
        // Prose and unknown files stay plain.
        #expect(DiffHighlight.language(of: "README.md") == nil)
    }

    @Test func lockGeneratedAndLongDataFilesStartFolded() {
        #expect("web/package-lock.json".isGeneratedOrLockFile && "Cargo.lock".isGeneratedOrLockFile)
        #expect("vendor/lib/a.go".isGeneratedOrLockFile && "api/v1.pb.go".isGeneratedOrLockFile)
        #expect(!"src/lockfile.rs".isGeneratedOrLockFile && !"package.json".isGeneratedOrLockFile)
        #expect("data/runs/a.json".isDataFile && !"src/main.swift".isDataFile)
    }

    @Test func theReviewMenuStepsThroughFiles() {
        let paths = ["a", "b", "c"]
        #expect(DiffPane.path(after: nil, by: 1, in: paths) == "a")
        #expect(DiffPane.path(after: "a", by: 1, in: paths) == "b")
        #expect(DiffPane.path(after: "b", by: -1, in: paths) == "a")
        #expect(DiffPane.path(after: "c", by: 1, in: paths) == nil)
    }

    @Test func newsIsARequestAReRequestOrChecksThatStartedFailing() {
        func pr(_ number: UInt64, checks: CheckState? = .success) -> PullRequest {
            PullRequest(number: number, title: "T\(number)", url: "u\(number)", repo: "acme/api", author: "dana",
                        isDraft: false, reviewDecision: nil, checkState: checks, updatedAt: Date(), additions: 0, deletions: 0)
        }
        func dashboard(authored: [PullRequest] = [], waiting: [PullRequest] = [], reviewed: [PullRequest] = []) -> Dashboard {
            Dashboard(viewerLogin: "me", authored: authored, reviewRequested: waiting, reviewedByMe: reviewed, warnings: [])
        }
        let old = dashboard(authored: [pr(1)], waiting: [pr(2)], reviewed: [pr(3)])
        let new = dashboard(authored: [pr(1, checks: .failure), pr(9, checks: .failure)], waiting: [pr(2), pr(3), pr(4)])
        #expect(new.news(since: old).map(\.title) == ["Review requested again", "Review requested", "Checks failing"])
        #expect(new.news(since: old).map(\.url) == ["u3", "u4", "u1"])
        #expect(new.news(since: new).isEmpty)
    }
}

import Testing
@testable import GHViewer

struct RecentlyUsedTests {
    @Test func dropsTheLeastRecentlyUsedBeyondTheLimit() {
        var used = RecentlyUsed()
        for key in ["a", "b", "c"] {
            #expect(used.use(key, limit: 3) { _ in true }.isEmpty)
        }
        // Using "a" again makes "b" the oldest.
        #expect(used.use("a", limit: 3) { _ in true }.isEmpty)
        #expect(used.use("d", limit: 3) { _ in true } == ["b"])
        #expect(used.keys == ["c", "a", "d"])
    }

    @Test func skipsKeysItMayNotDrop() {
        var used = RecentlyUsed()
        for key in ["busy", "b"] {
            _ = used.use(key, limit: 2) { _ in true }
        }
        #expect(used.use("c", limit: 2) { $0 != "busy" } == ["b"])
        #expect(used.keys == ["busy", "c"])
        // A busy key stays even when that leaves more than the limit.
        #expect(used.use("d", limit: 1) { $0 != "busy" } == ["c"])
        #expect(used.keys == ["busy", "d"])
    }
}

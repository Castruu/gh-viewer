import Testing
@testable import GHViewer

struct RecentlyUsedTests {
    @Test func dropsTheLeastRecentlyUsedBeyondTheLimit() {
        var used = RecentlyUsed()
        for key in ["a", "b", "c"] {
            used.use(key)
        }
        // Using "a" again makes "b" the oldest.
        used.use("a")
        used.use("d")
        #expect(used.trim(limit: 3) { _ in true } == ["b"])
        #expect(used.keys == ["c", "a", "d"])
        used.remove("a")
        #expect(used.keys == ["c", "d"])
    }

    @Test func skipsKeysItMayNotDropUntilTheyCanBe() {
        var used = RecentlyUsed()
        for key in ["busy", "b", "c"] {
            used.use(key)
        }
        #expect(used.trim(limit: 1) { $0 != "busy" } == ["b"])
        #expect(used.keys == ["busy", "c"])
        // Once "busy" may go, trimming again drops it without reordering.
        #expect(used.trim(limit: 1) { _ in true } == ["busy"])
        #expect(used.keys == ["c"])
    }
}

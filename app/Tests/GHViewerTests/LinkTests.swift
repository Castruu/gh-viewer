import Foundation
import Testing
@testable import GHViewer

struct LinkTests {
    @Test func linksOpenOnlyOnTheWeb() {
        #expect(URL(string: "https://github.com/o/r/pull/7")!.isWebLink)
        #expect(URL(string: "HTTP://example.com")!.isWebLink)
        #expect(!URL(string: "file:///etc/passwd")!.isWebLink)
        #expect(!URL(string: "x-apple.systempreferences:com.apple.preference.security")!.isWebLink)
        #expect(!URL(string: "src/a.rs")!.isWebLink)
    }
}

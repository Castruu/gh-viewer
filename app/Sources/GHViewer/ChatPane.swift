import SwiftUI

/// The agent chat for the selected PR, in the window's inspector. Stub until the chat view lands.
struct ChatPane: View {
    /// `nil` when no PR is selected.
    let review: PRReview?

    var body: some View {
        ContentUnavailableView {
            Label("Agent Chat Coming Soon", systemImage: "bubble.left.and.bubble.right")
        } description: {
            if let review {
                Text("Ask your coding agent about #\(review.number) here.")
            } else {
                Text("Select a pull request to ask your coding agent about it.")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

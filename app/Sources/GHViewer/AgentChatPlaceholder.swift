import SwiftUI

/// Stands in for the agent chat in the review window's inspector until the core's agent API
/// (`agent.rs`) is in the bindings. Replace it with the chat for the selected PR's session.
struct AgentChatPlaceholder: View {
    /// `nil` when no PR is selected.
    let pullRequest: PullRequest?

    var body: some View {
        ContentUnavailableView {
            Label("Agent Chat Coming Soon", systemImage: "bubble.left.and.bubble.right")
        } description: {
            if let pullRequest {
                Text("Ask your coding agent about #\(pullRequest.number) here.")
            } else {
                Text("Select a pull request to ask your coding agent about it.")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

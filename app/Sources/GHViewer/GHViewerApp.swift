import SwiftUI

@main
struct GHViewerApp: App {
    @State private var model = AppModel()
    @Environment(\.openWindow) private var openWindow

    var body: some Scene {
        MenuBarExtra {
            MenuView(model: model)
        } label: {
            MenuBarLabel(reviewCount: model.dashboard?.reviewRequested.count,
                         hasError: model.errorMessage != nil)
                // Development aid, for screenshots without clicking the menu bar:
                // `open build/GHViewer.app --args --open-review-window [<PR URL to select>]`.
                .onAppear {
                    let arguments = CommandLine.arguments
                    if let index = arguments.firstIndex(of: "--open-review-window") {
                        model.selectedPullRequestURL = arguments.dropFirst(index + 1).first
                        openWindow.reviewWindow()
                    }
                }
        }
        .menuBarExtraStyle(.window)

        // Opened from the menu bar; ReviewWindow keeps it from opening by itself at launch.
        Window("GH Viewer", id: ReviewWindow.id) {
            ReviewWindow(model: model)
        }
    }
}

/// The icon in the menu bar, with the number of PRs waiting on your review.
private struct MenuBarLabel: View {
    /// `nil` until the first successful refresh.
    let reviewCount: Int?
    let hasError: Bool

    var body: some View {
        // A failed refresh only shows the warning icon when there's no data yet;
        // otherwise keep showing the last known count.
        if let reviewCount, reviewCount > 0 {
            // Image + Text in an HStack renders as a single menu bar item.
            HStack(spacing: 2) {
                Image(systemName: "arrow.triangle.pull")
                Text("\(reviewCount)")
            }
        } else if reviewCount == nil && hasError {
            Image(systemName: "exclamationmark.triangle")
        } else {
            Image(systemName: "arrow.triangle.pull")
        }
    }
}

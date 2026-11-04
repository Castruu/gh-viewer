import SwiftUI

@main
struct GitHubViewerApp: App {
    @State private var model = AppModel()

    var body: some Scene {
        MenuBarExtra {
            MenuView(model: model)
        } label: {
            MenuBarLabel(reviewCount: model.dashboard?.reviewRequested.count ?? 0,
                         hasError: model.errorMessage != nil)
        }
        .menuBarExtraStyle(.window)
    }
}

/// The icon in the menu bar, with the number of PRs waiting on your review.
private struct MenuBarLabel: View {
    let reviewCount: Int
    let hasError: Bool

    var body: some View {
        if hasError {
            Image(systemName: "exclamationmark.triangle")
        } else if reviewCount > 0 {
            // Image + Text in an HStack renders as a single menu bar item.
            HStack(spacing: 2) {
                Image(systemName: "arrow.triangle.pull")
                Text("\(reviewCount)")
            }
        } else {
            Image(systemName: "arrow.triangle.pull")
        }
    }
}

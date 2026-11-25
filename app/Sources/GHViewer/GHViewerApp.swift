import SwiftUI
import UserNotifications

@main
struct GHViewerApp: App {
    @NSApplicationDelegateAdaptor private var appDelegate: AppDelegate
    @State private var model = AppModel()
    @State private var reviews = ReviewStore()
    @Environment(\.openWindow) private var openWindow

    var body: some Scene {
        MenuBarExtra {
            MenuView(model: model)
                .themed()
        } label: {
            MenuBarLabel(reviewCount: model.dashboard?.reviewRequested.count,
                         hasError: model.errorMessage != nil)
                // Opening the app again from Finder or Spotlight shows the review window.
                // A clicked notification carries the PR it's about.
                .onReceive(NotificationCenter.default.publisher(for: AppDelegate.reopen)) { reopen in
                    if let url = reopen.object as? String {
                        model.selectedPullRequestURL = url
                    }
                    openWindow.reviewWindow()
                }
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
            ReviewWindow(model: model, reviews: reviews)
        }
        // Room for the sidebar, the diff and the agent chat side by side.
        .defaultSize(width: 1280, height: 800)
        // The window says what it is; the name only takes room from the toolbar.
        .windowToolbarStyle(.unified(showsTitle: false))
        .commands { ReviewCommands() }

        Settings {
            ThemeSettings()
                .themed()
        }
    }
}

/// Forwards "reopen" (launching the app again while it runs) to SwiftUI,
/// which has no scene API for it.
final class AppDelegate: NSObject, NSApplicationDelegate {
    static let reopen = Notification.Name("GHViewerReopen")

    /// Applies a saved Light or Dark choice before any window shows, so nothing flashes in the wrong one.
    func applicationDidFinishLaunching(_ notification: Notification) {
        let saved = UserDefaults.standard.string(forKey: ThemeKey.appearance) ?? ""
        NSApp.appearance = Appearance(rawValue: saved)?.nsAppearance
        UNUserNotificationCenter.current().delegate = self
        AppIcon(rawValue: UserDefaults.standard.string(forKey: AppIcon.key) ?? "")?.apply()
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        NotificationCenter.default.post(name: Self.reopen, object: nil)
        return false
    }
}

extension AppDelegate: UNUserNotificationCenterDelegate {
    /// Clicking a notification opens its PR in the review window.
    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let url = response.notification.request.content.userInfo["url"] as? String
        await MainActor.run {
            NotificationCenter.default.post(name: Self.reopen, object: url)
        }
    }

    /// Shown even while the review window is in front: the news may be about another PR.
    func userNotificationCenter(_ center: UNUserNotificationCenter,
                                willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner]
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

import Foundation
import UserNotifications

/// Something in your queue worth an interruption.
struct QueueNews: Equatable {
    let title: String
    let body: String
    /// The PR to open when the notification is clicked.
    let url: String
}

extension Dashboard {
    /// What happened since `old`: a review was requested of you, or asked for again after
    /// you'd reviewed, or the checks on one of your own PRs started failing.
    func news(since old: Dashboard) -> [QueueNews] {
        let wasWaiting = Set(old.reviewRequested.map(\.url))
        let hadReviewed = Set(old.reviewedByMe.map(\.url))
        var news = reviewRequested.filter { !wasWaiting.contains($0.url) }.map { pullRequest in
            QueueNews(title: hadReviewed.contains(pullRequest.url) ? "Review requested again" : "Review requested",
                      body: "\(pullRequest.repo) #\(pullRequest.number): \(pullRequest.title)", url: pullRequest.url)
        }
        let checks = Dictionary(old.authored.map { ($0.url, $0.checkState) }) { first, _ in first }
        for pullRequest in authored where pullRequest.checkState == .failure {
            // Only a PR that was there before and wasn't failing: a new failing PR is one you just opened.
            // It doesn't repeat while the checks stay failing, and it does tell you again when a
            // re-run or a new push fails after the checks had been running or passing.
            if let before = checks[pullRequest.url], before != .failure {
                news.append(QueueNews(title: "Checks failing",
                                      body: "\(pullRequest.repo) #\(pullRequest.number): \(pullRequest.title)",
                                      url: pullRequest.url))
            }
        }
        return news
    }
}

enum Notifier {
    /// `UserDefaults` key of the Settings switch; on unless turned off.
    static let key = "notifies"

    static func post(_ news: [QueueNews]) {
        guard !news.isEmpty, UserDefaults.standard.object(forKey: key) as? Bool ?? true else { return }
        let center = UNUserNotificationCenter.current()
        // Asked the first time there's something to say, when it's clear what the permission is for.
        center.requestAuthorization(options: [.alert]) { granted, _ in
            guard granted else { return }
            for item in news {
                let content = UNMutableNotificationContent()
                content.title = item.title
                content.body = item.body
                content.userInfo = ["url": item.url]
                center.add(UNNotificationRequest(identifier: item.title + item.url, content: content, trigger: nil))
            }
        }
    }
}

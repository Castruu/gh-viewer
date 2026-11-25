import SwiftUI

/// The review window's sidebar: every open PR you're involved in, grouped by what it needs from you.
/// The open PR's files have their own column, beside the diff.
struct ReviewSidebar: View {
    @Bindable var model: AppModel
    /// The PRs you've already reviewed are the least urgent, so they start folded.
    @State private var collapsed: Set<String> = [SidebarSection.reviewed]
    // The filter's checkboxes. What needs your review always shows: that's what the app is for.
    @AppStorage("sidebarShowsYours") private var showsYours = true
    @AppStorage("sidebarShowsReviewed") private var showsReviewed = true
    @AppStorage("sidebarHidesViewedFiles") private var hidesViewedFiles = false

    var body: some View {
        pullRequests
    }

    /// Less to read: untick what you don't need to see. The icon fills in while something is hidden.
    private var filterMenu: some View {
        Menu {
            Section("Pull Requests") {
                Toggle(SidebarSection.yours, isOn: $showsYours)
                Toggle(SidebarSection.reviewed, isOn: $showsReviewed)
            }
            Section("Files") {
                Toggle("Hide Viewed Files", isOn: $hidesViewedFiles)
            }
        } label: {
            let isFiltering = !showsYours || !showsReviewed || hidesViewedFiles
            Label("Filter", systemImage: isFiltering ? "line.3.horizontal.decrease.circle.fill"
                                                    : "line.3.horizontal.decrease.circle")
                .labelStyle(.iconOnly)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Choose what the sidebar shows")
    }

    @ViewBuilder
    private var pullRequests: some View {
        if let dashboard = model.dashboard {
            let sections = dashboard.sidebarSections.filter {
                (showsYours || $0.title != SidebarSection.yours) && (showsReviewed || $0.title != SidebarSection.reviewed)
            }
            // A VStack rather than safe area insets, so rows never scroll under the banner or status.
            VStack(spacing: 0) {
                if !dashboard.warnings.isEmpty {
                    WarningBanner(warnings: dashboard.warnings)
                }
                List(selection: $model.selectedPullRequestURL) {
                    ForEach(sections) { section in
                        Section(isExpanded: isExpanded(section.title)) {
                            ForEach(section.pullRequests, id: \.url) { pullRequest in
                                PRRowLabel(pullRequest: pullRequest)
                                    .padding(.vertical, 4)
                            }
                        } header: {
                            Text("\(section.title)  \(Text("\(section.pullRequests.count)").foregroundStyle(.tertiary))")
                        }
                    }
                }
                .overlay {
                    if sections.isEmpty {
                        ContentUnavailableView("No Open Pull Requests", systemImage: "checkmark.circle",
                                               description: Text("Nothing waiting on you, and nothing of yours open."))
                    }
                }
                HStack {
                    RefreshStatus(model: model)
                        .font(.caption)
                    Spacer()
                    filterMenu
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
            // A PR picked in the menu bar may sit in a collapsed section.
            .onChange(of: model.selectedPullRequestURL) { _, url in
                if let section = sections.first(where: { $0.pullRequests.contains { $0.url == url } }) {
                    collapsed.remove(section.title)
                }
            }
        } else if let errorMessage = model.errorMessage {
            ErrorView(message: errorMessage, isRetrying: model.isRefreshing) {
                Task { await model.refresh() }
            }
            .frame(maxHeight: .infinity)
        } else {
            ProgressView("Loading pull requests…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func isExpanded(_ title: String) -> Binding<Bool> {
        Binding {
            !collapsed.contains(title)
        } set: { isExpanded in
            if isExpanded {
                collapsed.remove(title)
            } else {
                collapsed.insert(title)
            }
        }
    }
}

/// One sidebar section: the PRs that need the same thing from you.
struct SidebarSection: Identifiable {
    static let yours = "Your pull requests"
    static let reviewed = "Reviewed by you"

    let title: String
    let pullRequests: [PullRequest]

    var id: String { title }
}

extension Dashboard {
    /// What needs you first: your review, then your own PRs, then the ones you've already reviewed.
    /// A PR can be in several of GitHub's lists; it shows once, in the first.
    var sidebarSections: [SidebarSection] {
        var seen = Set<String>()
        return [("Needs your review", reviewRequested), (SidebarSection.yours, authored), (SidebarSection.reviewed, reviewedByMe)]
            .map { title, list in SidebarSection(title: title, pullRequests: list.filter { seen.insert($0.url).inserted }) }
            .filter { !$0.pullRequests.isEmpty }
    }
}

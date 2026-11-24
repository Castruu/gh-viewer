import SwiftUI

/// Stands in for the selected PR's diff until the core's diff API (`checkout.rs`) is in the bindings.
/// Replace it with the real file list and diff view.
struct DiffPlaceholder: View {
    let pullRequest: PullRequest

    var body: some View {
        ContentUnavailableView("Diff Coming Soon", systemImage: "plus.forwardslash.minus",
                               description: Text("The changes in #\(pullRequest.number) will show here."))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

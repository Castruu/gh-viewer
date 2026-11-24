import SwiftUI

/// The selected PR's changed files and diff. Stub until the diff view lands.
struct DiffPane: View {
    let review: PRReview

    var body: some View {
        ContentUnavailableView("Diff Coming Soon", systemImage: "plus.forwardslash.minus",
                               description: Text("The changes in #\(review.number) will show here."))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

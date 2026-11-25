import SwiftUI

/// What the Review menu can do in the window in front, set by its diff.
struct ReviewActions {
    let nextFile: () -> Void
    let previousFile: () -> Void
    let toggleViewed: () -> Void
    let comment: () -> Void
    let askAgent: () -> Void
    let submit: () -> Void
}

private struct ReviewActionsKey: FocusedValueKey {
    typealias Value = ReviewActions
}

extension FocusedValues {
    var reviewActions: ReviewActions? {
        get { self[ReviewActionsKey.self] }
        set { self[ReviewActionsKey.self] = newValue }
    }
}

/// The Review menu: a review from the keyboard. Every shortcut has a modifier, so
/// typing in the chat or a comment never triggers one.
struct ReviewCommands: Commands {
    @FocusedValue(\.reviewActions) private var actions
    @AppStorage(DiffFileList.showsKey) private var showsFiles = true

    var body: some Commands {
        CommandMenu("Review") {
            Button(showsFiles ? "Hide File Tree" : "Show File Tree") { showsFiles.toggle() }
                .keyboardShortcut("f", modifiers: [.command, .shift])
            Divider()
            Group {
                Button("Next File") { actions?.nextFile() }
                    .keyboardShortcut(.downArrow, modifiers: [.command, .option])
                Button("Previous File") { actions?.previousFile() }
                    .keyboardShortcut(.upArrow, modifiers: [.command, .option])
                Button("Mark File as Viewed") { actions?.toggleViewed() }
                    .keyboardShortcut("v", modifiers: [.command, .option])
                Divider()
                Button("Comment on Selection") { actions?.comment() }
                    .keyboardShortcut("c", modifiers: [.command, .option])
                Button("Ask Agent About Selection") { actions?.askAgent() }
                    .keyboardShortcut("a", modifiers: [.command, .option])
                Divider()
                Button("Submit Review…") { actions?.submit() }
                    .keyboardShortcut(.return, modifiers: [.command, .shift])
            }
            .disabled(actions == nil)
        }
    }
}

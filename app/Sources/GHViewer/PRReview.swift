import Foundation
import Observation

/// Everything the review window holds for one PR: the diff, drafts, the line
/// selection, and the agent chat. Views read it and call its actions; all GitHub,
/// git and agent work happens in the Rust core.
@MainActor
@Observable
final class PRReview {
    enum LoadState: Equatable {
        case idle
        case loading
        case loaded
        case failed(String)
    }

    let repo: String
    let number: UInt64
    let url: String

    private(set) var loadState: LoadState = .idle
    /// The last successful load. Kept while reloading or after a failed reload.
    private(set) var opened: OpenedReview?
    private(set) var drafts: [DraftComment] = []
    /// Set when an action fails (viewed toggle, draft edit, submit); shown by the views.
    var actionError: String?
    /// The diff lines the user selected. Questions to the agent include them.
    var selection: Selection?
    var chat = ChatState()
    /// Set by Stop before the agent's session exists; the question is then dropped instead of sent.
    private(set) var isCancelPending = false
    /// Changed by the diff to show the chat and focus its composer (e.g. "Ask about these lines").
    var chatFocusRequest: UUID?
    private(set) var isSubmitting = false
    /// Set when a saved chat exists but couldn't be read, so saving would overwrite it.
    @ObservationIgnored private var isChatArchiveUnreadable = false

    @ObservationIgnored private unowned let store: ReviewStore

    init(pullRequest: PullRequest, store: ReviewStore) {
        repo = pullRequest.repo
        number = pullRequest.number
        url = pullRequest.url
        self.store = store
        reloadDrafts()
        restoreChat()
    }

    /// True while drafts hold the diff on an older commit than the PR's latest push.
    var hasNewerCommits: Bool {
        guard let checkout = opened?.checkout else { return false }
        return checkout.headOid != checkout.latestHeadOid
    }

    /// True while a reload runs over a diff already shown. Drafting waits: the reload may move the diff to another commit.
    var isReloading: Bool {
        loadState == .loading && opened != nil
    }

    static let reloadingHelp = "Wait for the reload to finish; the diff may move to another commit"

    // MARK: Loading

    /// Fetches the PR, updates its checkout and diffs it. The first open of a repo clones it.
    func load() async {
        guard loadState != .loading else { return }
        loadState = .loading
        do {
            opened = try await store.hub.open(repo: repo, number: number)
            loadState = .loaded
        } catch {
            loadState = .failed(error.coreMessage)
        }
        reloadDrafts()
    }

    func reloadDrafts() {
        do {
            drafts = try store.drafts.list(repo: repo, number: number)
        } catch {
            actionError = error.coreMessage
        }
    }

    // MARK: Viewed

    func isViewed(_ path: String) -> Bool {
        opened?.detail.files.first { $0.path == path }?.viewed == .viewed
    }

    /// Updates GitHub's "Viewed" checkbox, showing the change right away and undoing it if GitHub refuses.
    func setViewed(_ path: String, _ viewed: Bool) async {
        guard let id = opened?.detail.id else { return }
        let previous = opened?.detail.files.first { $0.path == path }?.viewed
        updateViewed(path, viewed ? .viewed : .unviewed)
        do {
            try await store.github.setFileViewed(pullRequestId: id, path: path, viewed: viewed)
        } catch {
            if let previous { updateViewed(path, previous) }
            actionError = error.coreMessage
        }
    }

    private func updateViewed(_ path: String, _ state: ViewedState) {
        guard let index = opened?.detail.files.firstIndex(where: { $0.path == path }) else { return }
        opened?.detail.files[index].viewed = state
    }

    // MARK: Drafts

    /// Adds your own comment on `selection`. The views only offer commentable lines.
    /// False if it failed, with the reason in `actionError`; the same goes for the other draft actions.
    @discardableResult
    func addDraft(on selection: Selection, body: String) -> Bool {
        guard !isReloading, let commit = opened?.checkout.headOid else { return false }
        let single = selection.startLine == selection.line && selection.startSide == selection.side
        let draft = NewDraft(
            path: selection.path,
            line: selection.line,
            side: selection.side,
            startLine: single ? nil : selection.startLine,
            startSide: single ? nil : selection.startSide,
            body: body,
            author: .you,
            commitOid: commit
        )
        return perform { _ = try self.store.drafts.add(repo: self.repo, number: self.number, draft: draft) }
    }

    @discardableResult
    func updateDraft(id: String, body: String) -> Bool {
        perform { try self.store.drafts.updateBody(repo: self.repo, number: self.number, id: id, body: body) }
    }

    @discardableResult
    func removeDraft(id: String) -> Bool {
        perform { try self.store.drafts.remove(repo: self.repo, number: self.number, id: id) }
    }

    /// Deletes every draft, which also unpins the diff on the next load.
    func discardDrafts() async {
        perform { try self.store.drafts.clear(repo: self.repo, number: self.number) }
        await load()
    }

    /// Submits the drafts as one review. On success the drafts are gone and the
    /// PR reloads at its latest head. Returns the review's URL.
    @discardableResult
    func submit(event: ReviewEvent, body: String) async -> String? {
        isSubmitting = true
        defer { isSubmitting = false }
        do {
            let reviewURL = try await store.hub.submit(repo: repo, number: number, event: event, body: body)
            reloadDrafts()
            await load()
            return reviewURL
        } catch {
            actionError = error.coreMessage
            return nil
        }
    }

    @discardableResult
    private func perform(_ action: () throws -> Void) -> Bool {
        // The store's listener also reloads, but only after a main-actor hop; reload now so the UI doesn't lag.
        defer { reloadDrafts() }
        do {
            try action()
            return true
        } catch {
            actionError = error.coreMessage
            return false
        }
    }

    // MARK: Agent chat

    /// Sends a question to the chosen agent, starting a session in the PR's
    /// worktree if needed. Includes the current selection when `withSelection`.
    func ask(_ question: String, withSelection: Bool) async {
        let question = question.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !question.isEmpty, !chat.isRunning else { return }
        let selection = withSelection ? self.selection : nil
        let kind = chat.agent
        if let previous = chat.items.last(where: \.isUser)?.userAgent, previous != kind {
            chat.append(.info("\(kind.displayName) hasn't seen the answers above."))
        }
        chat.append(.user(text: question, selection: selection, agent: kind))
        chat.isRunning = true
        isCancelPending = false
        defer {
            chat.isRunning = false
            chat.runningSession = nil
            isCancelPending = false
            saveChat()
        }
        do {
            let sessionId = try await session(for: kind)
            chat.runningSession = sessionId
            if isCancelPending {
                chat.append(.stopped)
                return
            }
            let stopReason = try await store.hub.ask(sessionId: sessionId, selection: selection, question: question)
            if stopReason == "cancelled" {
                chat.append(.stopped)
            } else if stopReason != "end_turn" {
                chat.append(.notice("The agent stopped: \(stopReason)."))
            }
        } catch {
            chat.append(.notice(error.coreMessage))
        }
    }

    /// Stops the turn. Before the session exists, the question is dropped once it does.
    func cancel() {
        guard let sessionId = chat.sessions[chat.agent] else {
            isCancelPending = chat.isRunning
            return
        }
        store.hub.cancel(sessionId: sessionId)
    }

    /// Answers a permission prompt in the chat; `optionId` nil dismisses it.
    func respond(toPermission requestId: String, optionId: String?) {
        store.hub.respondPermission(requestId: requestId, optionId: optionId)
        chat.resolvePermission(requestId, optionId: optionId)
        saveChat()
    }

    /// Deletes the saved transcript and empties the chat. The next question starts a fresh session.
    func clearChat() {
        guard !chat.isRunning else { return }
        // Otherwise the old sessions' MCP endpoints would keep accepting drafts.
        chat.sessions.values.forEach { store.hub.closeAgent(sessionId: $0) }
        chat.clear()
        do {
            try store.chats.delete(repo: repo, number: number)
            isChatArchiveUnreadable = false
        } catch {
            actionError = error.coreMessage
        }
    }

    /// Saves a turn that's still streaming; finished turns were saved when they ended.
    func saveChatIfRunning() {
        if chat.isRunning {
            saveChat()
        }
    }

    private func saveChat() {
        guard !isChatArchiveUnreadable else { return }
        do {
            try store.chats.save(chat.items, repo: repo, number: number)
        } catch {
            actionError = error.coreMessage
        }
    }

    /// Shows the transcript saved before a restart, and where the new session starts.
    private func restoreChat() {
        do {
            guard let saved = try store.chats.load(repo: repo, number: number) else { return }
            chat.restore(saved.items)
            chat.agent = saved.items.last(where: \.isUser)?.userAgent ?? chat.agent
            let date = saved.savedAt.formatted(date: .abbreviated, time: .shortened)
            chat.append(.info("Conversation from \(date). \(chat.agent.displayName) starts a new session from here and doesn't remember it."))
        } catch {
            isChatArchiveUnreadable = error is ChatArchive.Unreadable
            chat.append(.notice(error.coreMessage))
        }
    }

    private func session(for kind: AgentKind) async throws -> String {
        if let existing = chat.sessions[kind] {
            return existing
        }
        if opened == nil {
            await load()
            // load() returns at once while another load runs, such as the diff's first one.
            // shortcut: polls; await a stored load Task instead if more callers need this.
            while opened == nil && loadState == .loading {
                try await Task.sleep(for: .milliseconds(100))
            }
        }
        chat.isStarting = true
        defer { chat.isStarting = false }
        let sessionId = try await store.hub.startAgent(kind: kind, repo: repo, number: number)
        chat.sessions[kind] = sessionId
        return sessionId
    }

    func apply(_ event: AgentEvent, sessionId: String) {
        chat.apply(event, sessionId: sessionId)
    }
}

extension Error {
    /// The Rust error's own text for `CoreError`, otherwise the system description.
    var coreMessage: String {
        (self as? CoreError)?.message ?? localizedDescription
    }
}

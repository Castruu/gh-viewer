//! ACP client: runs the user's coding agents and streams their output to the app.
//!
//! One adapter process runs per [`AgentKind`], started on the first session
//! and shared by every session of that kind. Each PR gets its own session,
//! with `cwd` set to the PR's worktree.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, BoxFuture, Client, DynConnectTo, ErrorCode, LineDirection,
};
use tokio::sync::oneshot;

use crate::{CoreError, runtime};

/// Covers `initialize` and `session/new`. The first `npx -y` downloads the
/// adapter, which can take a while on a slow connection.
const SETUP_TIMEOUT: Duration = Duration::from_secs(120);
/// An rc file that waits for input or a slow plugin manager must not stall
/// every agent start.
const SHELL_ENV_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a cancelled turn may take to end before `prompt` stops waiting for the agent.
const CANCEL_GRACE: Duration = Duration::from_secs(5);
const STDERR_LINES: usize = 50;
/// Brackets `env -0` output so noise printed by interactive rc files is ignored.
const ENV_MARKER: &str = "__GHV_SHELL_ENV_5f1c__";

/// The name the app's own MCP server must be registered under in `session/new`.
pub(crate) const MCP_SERVER_NAME: &str = "gh-viewer";
/// Its tools. They're local and reversible (drafts stay local until the user
/// submits them), so their permission requests are allowed once without asking.
pub(crate) const MCP_TOOLS: [&str; 3] = ["get_pr_context", "list_review_threads", "draft_comment"];

/// A coding agent the app can run through its ACP adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    fn package(self) -> &'static str {
        match self {
            Self::Claude => "@agentclientprotocol/claude-agent-acp",
            Self::Codex => "@agentclientprotocol/codex-acp",
        }
    }

    fn login_command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex login",
        }
    }
}

/// Receives everything agents send. Called from background threads; the
/// implementation hops to the main actor itself.
#[uniffi::export(with_foreign)]
pub trait AgentListener: Send + Sync {
    fn on_event(&self, session_id: String, event: AgentEvent);
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum AgentEvent {
    MessageChunk {
        text: String,
    },
    ThoughtChunk {
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        /// ACP tool kind: "read", "edit", "execute", "search", "other", …
        kind: String,
        status: ToolStatus,
    },
    /// `text` is a plain-text rendering of the update's content, if any.
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
        text: Option<String>,
    },
    /// The full current plan; each one replaces the previous.
    Plan {
        entries: Vec<PlanEntry>,
    },
    /// Answer with [`AgentHub::respond_permission`].
    PermissionRequest {
        request_id: String,
        /// The `ToolCall` this asks about, so the UI can show it next to the call.
        tool_call_id: String,
        title: String,
        options: Vec<PermissionChoice>,
    },
    /// The agent process exited; sessions on it are gone.
    Disconnected {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PlanEntry {
    pub content: String,
    /// "pending", "in_progress" or "completed"
    pub status: String,
}

/// One of the agent's own options for a permission request.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PermissionChoice {
    pub id: String,
    pub name: String,
    /// "allow_once", "allow_always", "reject_once" or "reject_always"
    pub kind: String,
}

/// An MCP server the agent reaches over streamable HTTP.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct McpServerHttp {
    pub name: String,
    pub url: String,
    pub headers: Vec<HttpHeader>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// Starts the transport to an agent. Swapped for an in-process agent in tests.
type Launcher =
    fn(AgentKind, Stderr) -> BoxFuture<'static, Result<DynConnectTo<Client>, CoreError>>;

/// Runs the user's agents and routes their sessions, updates and permission
/// requests.
#[derive(uniffi::Object)]
pub struct AgentHub {
    listener: Arc<dyn AgentListener>,
    shared: Arc<Mutex<Shared>>,
    /// One slot per [`AgentKind`]. The async lock also makes concurrent first
    /// sessions share one start instead of racing to spawn two processes.
    processes: [tokio::sync::Mutex<Option<Process>>; 2],
    launch: Launcher,
}

#[derive(Default)]
struct Shared {
    next_id: u64,
    /// Processes whose connection is still running.
    live: HashSet<u64>,
    sessions: HashMap<String, Session>,
    permissions: HashMap<String, PendingPermission>,
    /// (session id, tool call id) of running Codex calls to the app's own MCP tools.
    own_codex_calls: HashSet<(String, String)>,
    /// Output so far of running Codex commands, by (session id, tool call id).
    codex_output: HashMap<(String, String), String>,
}

impl Shared {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

struct Session {
    kind: AgentKind,
    process: u64,
    cx: ConnectionTo<Agent>,
    /// Woken by `cancel`, so a turn the agent never ends can still stop.
    cancelled: Arc<tokio::sync::Notify>,
}

struct PendingPermission {
    session_id: String,
    /// Dropping it answers the request as cancelled.
    reply: oneshot::Sender<Option<String>>,
}

#[derive(Clone)]
struct Process {
    id: u64,
    cx: ConnectionTo<Agent>,
    mcp_http: bool,
    stderr: Stderr,
    /// The connection shuts the process down once every clone is dropped.
    _shutdown: Arc<oneshot::Sender<()>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl AgentHub {
    #[uniffi::constructor]
    pub fn new(listener: Arc<dyn AgentListener>) -> Self {
        Self::with_launcher(listener, launch_adapter)
    }

    /// Starts the agent process if needed and opens a session in `cwd`. Returns the session id.
    pub async fn new_session(
        &self,
        kind: AgentKind,
        cwd: String,
        mcp_servers: Vec<McpServerHttp>,
    ) -> Result<String, CoreError> {
        let process = self.process(kind).await?;
        if !mcp_servers.is_empty() && !process.mcp_http {
            return Err(CoreError::Agent(format!(
                "{}'s ACP adapter doesn't support HTTP MCP servers, which the app needs for review comments. Clear the npx cache (`rm -rf ~/.npm/_npx`) to get the latest adapter, then try again.",
                kind.name()
            )));
        }

        let servers = mcp_servers
            .into_iter()
            .map(|server| {
                let headers = server
                    .headers
                    .into_iter()
                    .map(|header| acp::HttpHeader::new(header.name, header.value))
                    .collect();
                acp::McpServer::Http(
                    acp::McpServerHttp::new(server.name, server.url).headers(headers),
                )
            })
            .collect();
        let mut request = acp::NewSessionRequest::new(PathBuf::from(cwd)).mcp_servers(servers);
        if kind == AgentKind::Claude {
            // The cwd holds the PR's code: load the user's own settings, never its hooks, MCP servers or permissions.
            let meta = serde_json::json!({ "claudeCode": { "options": { "settingSources": ["user"] } } });
            if let serde_json::Value::Object(meta) = meta {
                request = request.meta(meta);
            }
        }
        let response =
            tokio::time::timeout(SETUP_TIMEOUT, process.cx.send_request(request).block_task())
                .await
                .map_err(|_| {
                    CoreError::Agent(format!(
                        "{} didn't open a session within {}s.{}",
                        kind.name(),
                        SETUP_TIMEOUT.as_secs(),
                        process.stderr.tail()
                    ))
                })?
                .map_err(|error| match signed_out(kind, &error) {
                    Some(signed_out) => signed_out,
                    None => CoreError::Agent(format!(
                        "{} couldn't open a session: {error}{}",
                        kind.name(),
                        process.stderr.tail()
                    )),
                })?;

        let session_id = response.session_id.to_string();
        let mut shared = self.shared();
        // The process may have exited while session/new was in flight.
        if !shared.live.contains(&process.id) {
            return Err(CoreError::Agent(format!(
                "{} stopped while opening the session.",
                kind.name()
            )));
        }
        let session = Session {
            kind,
            process: process.id,
            cx: process.cx,
            cancelled: Arc::default(),
        };
        shared.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }

    /// Sends a user turn; resolves with the stop reason ("end_turn", "cancelled", …) when the turn ends.
    pub async fn prompt(&self, session_id: String, text: String) -> Result<String, CoreError> {
        let (kind, cx, cancelled) = self
            .shared()
            .sessions
            .get(&session_id)
            .map(|session| (session.kind, session.cx.clone(), session.cancelled.clone()))
            .ok_or_else(|| {
                CoreError::Agent("This agent session has ended. Start a new one.".into())
            })?;
        let request = acp::PromptRequest::new(session_id, vec![text.into()]);
        // An agent that never answers, e.g. one waiting on a hook, must not keep the turn after Stop.
        let gave_up = async {
            cancelled.notified().await;
            tokio::time::sleep(CANCEL_GRACE).await;
        };
        let response = tokio::select! {
            response = cx.send_request(request).block_task() => response,
            () = gave_up => return Ok("cancelled".into()),
        };
        let response = response
            .map_err(|error| {
                signed_out(kind, &error)
                    .unwrap_or_else(|| CoreError::Agent(format!("{}: {error}", kind.name())))
            })?;
        Ok(wire_name(&response.stop_reason))
    }

    /// Asks the agent to stop the current turn. `prompt` then resolves with "cancelled".
    pub fn cancel(&self, session_id: String) {
        let mut shared = self.shared();
        if let Some(session) = shared.sessions.get(&session_id) {
            // Only fails if the connection is already gone, which ends the turn anyway.
            let _ = session
                .cx
                .send_notification(acp::CancelNotification::new(session_id.clone()));
            session.cancelled.notify_waiters();
        }
        // ACP requires answering the turn's open permission requests as cancelled.
        shared
            .permissions
            .retain(|_, pending| pending.session_id != session_id);
    }

    /// Answers a PermissionRequest. `None` means the user dismissed it (ACP "cancelled" outcome).
    pub fn respond_permission(&self, request_id: String, option_id: Option<String>) {
        if let Some(pending) = self.shared().permissions.remove(&request_id) {
            // The agent may have stopped waiting; nothing to do then.
            let _ = pending.reply.send(option_id);
        }
    }
}

impl AgentHub {
    fn with_launcher(listener: Arc<dyn AgentListener>, launch: Launcher) -> Self {
        Self {
            listener,
            shared: Arc::default(),
            processes: Default::default(),
            launch,
        }
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    /// The running process for `kind`, started if there is none.
    async fn process(&self, kind: AgentKind) -> Result<Process, CoreError> {
        let mut slot = self.processes[kind as usize].lock().await;
        if let Some(process) = slot.as_ref()
            && self.shared().live.contains(&process.id)
        {
            return Ok(process.clone());
        }
        *slot = None;
        let process = self.start(kind).await?;
        *slot = Some(process.clone());
        Ok(process)
    }

    async fn start(&self, kind: AgentKind) -> Result<Process, CoreError> {
        let stderr = Stderr::default();
        let transport = (self.launch)(kind, stderr.clone()).await?;
        let id = {
            let mut shared = self.shared();
            let id = shared.next_id();
            shared.live.insert(id);
            id
        };

        let (ready_tx, ready_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let connection =
            self.client(kind)
                .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                    let _ = ready_tx.send(cx.clone());
                    tokio::select! {
                        () = cx.incoming_closed() => {}
                        _ = shutdown_rx => {}
                    }
                    Ok(())
                });
        let (listener, shared, tail) = (self.listener.clone(), self.shared.clone(), stderr.clone());
        let exited = runtime().spawn(async move {
            let result = connection.await;
            let reason = match result {
                Ok(()) => String::new(),
                // The SDK's message for a crash already embeds stderr; keep only its first line.
                Err(error) => format!(" ({})", error.message.lines().next().unwrap_or_default()),
            };
            let message = format!("{} stopped{reason}.{}", kind.name(), tail.tail());
            let gone: Vec<String> = {
                let mut shared = lock(&shared);
                shared.live.remove(&id);
                let gone: Vec<String> = shared
                    .sessions
                    .iter()
                    .filter(|(_, session)| session.process == id)
                    .map(|(session_id, _)| session_id.clone())
                    .collect();
                for session_id in &gone {
                    shared.sessions.remove(session_id);
                }
                shared
                    .permissions
                    .retain(|_, pending| !gone.contains(&pending.session_id));
                shared
                    .own_codex_calls
                    .retain(|(session_id, _)| !gone.contains(session_id));
                shared
                    .codex_output
                    .retain(|(session_id, _), _| !gone.contains(session_id));
                gone
            };
            for session_id in gone {
                listener.on_event(
                    session_id,
                    AgentEvent::Disconnected {
                        message: message.clone(),
                    },
                );
            }
            message
        });

        let Ok(cx) = ready_rx.await else {
            // The transport failed before the connection came up, e.g. the spawn failed.
            return Err(CoreError::Agent(
                exited.await.unwrap_or_else(|e| e.to_string()),
            ));
        };
        let process = Process {
            id,
            cx,
            mcp_http: false,
            stderr,
            _shutdown: Arc::new(shutdown_tx),
        };

        let initialize = acp::InitializeRequest::new(ProtocolVersion::V1)
            // The defaults advertise no fs or terminal support, so agents use their own tools in cwd.
            .client_capabilities(acp::ClientCapabilities::new())
            .client_info(acp::Implementation::new(
                "gh-viewer",
                env!("CARGO_PKG_VERSION"),
            ));
        let response = tokio::time::timeout(
            SETUP_TIMEOUT,
            process.cx.send_request(initialize).block_task(),
        )
        .await
        .map_err(|_| {
            CoreError::Agent(format!(
                "{} didn't start within {}s.{}",
                kind.name(),
                SETUP_TIMEOUT.as_secs(),
                process.stderr.tail()
            ))
        })?
        .map_err(|error| {
            CoreError::Agent(format!(
                "{} failed to start: {error}{}",
                kind.name(),
                process.stderr.tail()
            ))
        })?;
        Ok(Process {
            mcp_http: response.agent_capabilities.mcp_capabilities.http,
            ..process
        })
    }

    /// The client side of a connection: routes session updates and permission
    /// requests to the listener.
    fn client(
        &self,
        kind: AgentKind,
    ) -> agent_client_protocol::v1::Builder<
        Client,
        impl agent_client_protocol::HandleDispatchFrom<Agent> + 'static,
    > {
        let listener = self.listener.clone();
        let permission_listener = self.listener.clone();
        let shared = self.shared.clone();
        let notification_shared = self.shared.clone();
        Client
            .v1()
            .name("gh-viewer")
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    let session_id = notification.session_id.to_string();
                    let mut update = notification.update;
                    if kind == AgentKind::Codex {
                        let mut shared = lock(&notification_shared);
                        track_own_codex_call(&mut shared.own_codex_calls, &session_id, &update);
                        collect_codex_output(&mut shared.codex_output, &session_id, &mut update);
                    }
                    if let Some(event) = event_from_update(update) {
                        listener.on_event(session_id, event);
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: acp::RequestPermissionRequest,
                            responder,
                            cx: ConnectionTo<Agent>| {
                    let json = serde_json::to_value(&request).unwrap_or_default();
                    let allow = auto_allow(kind, &json, &mut lock(&shared).own_codex_calls);
                    if let Some(option_id) = allow {
                        let outcome = acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(option_id),
                        );
                        return responder.respond(acp::RequestPermissionResponse::new(outcome));
                    }
                    let session_id = request.session_id.to_string();
                    let (reply, answer) = oneshot::channel();
                    let request_id = {
                        let mut shared = lock(&shared);
                        // A per-run prefix, so a card from a saved chat can never match a new request.
                        static RUN: std::sync::LazyLock<String> = std::sync::LazyLock::new(crate::mcp::random_hex);
                        let request_id = format!("permission-{}-{}", *RUN, shared.next_id());
                        let pending = PendingPermission {
                            session_id: session_id.clone(),
                            reply,
                        };
                        shared.permissions.insert(request_id.clone(), pending);
                        request_id
                    };
                    let options = request
                        .options
                        .into_iter()
                        .map(|option| PermissionChoice {
                            id: option.option_id.to_string(),
                            name: option.name,
                            kind: wire_name(&option.kind),
                        })
                        .collect();
                    let tool_call_id = request.tool_call.tool_call_id.to_string();
                    let title = request
                        .tool_call
                        .fields
                        .title
                        .unwrap_or_else(|| "The agent asks for permission".into());
                    permission_listener.on_event(
                        session_id,
                        AgentEvent::PermissionRequest {
                            request_id,
                            tool_call_id,
                            title,
                            options,
                        },
                    );
                    // Waiting here would hold the dispatch loop, so the turn's
                    // other messages couldn't arrive until the user answers.
                    cx.spawn(async move {
                        let outcome = match answer.await {
                            Ok(Some(option_id)) => acp::RequestPermissionOutcome::Selected(
                                acp::SelectedPermissionOutcome::new(option_id),
                            ),
                            _ => acp::RequestPermissionOutcome::Cancelled,
                        };
                        // Fails only if the agent is gone, and then nobody is waiting.
                        let _ = responder.respond(acp::RequestPermissionResponse::new(outcome));
                        Ok(())
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
    }
}

/// The error to show when `error` means the user isn't signed in. Claude
/// reports it only on the first prompt; `session/new` succeeds regardless.
fn signed_out(kind: AgentKind, error: &agent_client_protocol::Error) -> Option<CoreError> {
    (error.code == ErrorCode::AUTH_REQUIRED).then(|| {
        CoreError::Agent(format!(
            "{} isn't signed in. Run `{}` in a terminal, log in, then try again.",
            kind.name(),
            kind.login_command()
        ))
    })
}

/// The option to answer with, without asking, when `request` (a
/// `session/request_permission`) is for one of the app's own MCP tools: its
/// `allow_once` option, never `allow_always`, which could write to the
/// user's agent config. Each adapter identifies an MCP tool differently.
fn auto_allow(
    kind: AgentKind,
    request: &serde_json::Value,
    own_codex_calls: &mut HashSet<(String, String)>,
) -> Option<String> {
    let tool_call = &request["toolCall"];
    let ours = match kind {
        // claude-agent-acp names the tool and its server in `_meta`. Source
        // "dynamic" is a server from session/new; a same-named server from the
        // user's own config has another source, so it isn't trusted.
        AgentKind::Claude => {
            let meta = &tool_call["_meta"]["claudeCode"];
            meta["mcpServer"]["name"] == MCP_SERVER_NAME
                && meta["mcpServer"]["source"] == "dynamic"
                && MCP_TOOLS
                    .iter()
                    .any(|tool| meta["toolName"] == format!("mcp__{MCP_SERVER_NAME}__{tool}"))
        }
        // codex-acp's approval names neither server nor tool, only the id of
        // the tool call it announced just before (see `track_own_codex_call`).
        AgentKind::Codex => {
            let key = (
                request["sessionId"].as_str().unwrap_or_default().to_owned(),
                tool_call["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            request["_meta"]["is_mcp_tool_approval"] == true && own_codex_calls.remove(&key)
        }
    };
    let options = request["options"].as_array().filter(|_| ours)?;
    let allow_once = options
        .iter()
        .find(|option| option["kind"] == "allow_once")?;
    allow_once["optionId"].as_str().map(str::to_owned)
}

/// Remembers Codex calls to the app's own MCP tools until they finish. Codex
/// fills `rawInput` from the MCP call itself, not from model output.
fn track_own_codex_call(
    own_codex_calls: &mut HashSet<(String, String)>,
    session_id: &str,
    update: &acp::SessionUpdate,
) {
    match update {
        acp::SessionUpdate::ToolCall(call) => {
            let call_json = serde_json::to_value(call).unwrap_or_default();
            let input = &call_json["rawInput"];
            if call_json["_meta"]["is_mcp_tool_call"] == true
                && input["server"] == MCP_SERVER_NAME
                && MCP_TOOLS.iter().any(|tool| input["tool"] == *tool)
            {
                own_codex_calls.insert((session_id.to_owned(), call.tool_call_id.to_string()));
            }
        }
        acp::SessionUpdate::ToolCallUpdate(update)
            if matches!(
                update.fields.status,
                Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
            ) =>
        {
            own_codex_calls.remove(&(session_id.to_owned(), update.tool_call_id.to_string()));
        }
        _ => {}
    }
}

/// codex-acp sends a command's output as chunks to append, in
/// `_meta.terminal_output_delta`, and only a terminal id as content. Passes
/// the output so far on as the update's content, which the UI shows.
fn collect_codex_output(
    outputs: &mut HashMap<(String, String), String>,
    session_id: &str,
    update: &mut acp::SessionUpdate,
) {
    let acp::SessionUpdate::ToolCallUpdate(update) = update else {
        return;
    };
    let key = (session_id.to_owned(), update.tool_call_id.to_string());
    let chunk = update
        .meta
        .as_ref()
        .and_then(|meta| meta.get("terminal_output_delta")?["data"].as_str());
    if let Some(chunk) = chunk {
        let output = outputs.entry(key.clone()).or_default();
        output.push_str(chunk);
        if update.fields.content.is_none() {
            update.fields.content = Some(vec![output.clone().into()]);
        }
    }
    if matches!(
        update.fields.status,
        Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
    ) {
        outputs.remove(&key);
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // Nothing in Shared can be left half-updated by a panic, so keep going.
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The last lines the agent wrote to stderr, for error messages.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<VecDeque<String>>>);

impl Stderr {
    fn push(&self, line: &str) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == STDERR_LINES {
            lines.pop_front();
        }
        lines.push_back(line.to_owned());
    }

    /// A suffix for error messages, empty if the agent wrote nothing.
    fn tail(&self) -> String {
        let lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.is_empty() {
            return String::new();
        }
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        format!("\n\nAgent output:\n{}", lines.join("\n"))
    }
}

/// Spawns the kind's ACP adapter through `npx` with the user's shell environment.
fn launch_adapter(
    kind: AgentKind,
    stderr: Stderr,
) -> BoxFuture<'static, Result<DynConnectTo<Client>, CoreError>> {
    Box::pin(async move {
        let env = login_env().await;
        let npx = env
            .get("PATH")
            .and_then(|path| std::env::split_paths(path).map(|dir| dir.join("npx")).find(|npx| npx.is_file()))
            .ok_or_else(|| {
                CoreError::Agent(
                    "Couldn't find `npx` on your login shell's PATH. Install Node.js, then try again.".into(),
                )
            })?;
        // AcpAgent runs the adapter in its own process group and kills the
        // group on drop, so the node process behind npx doesn't outlive us.
        let mut env = env.clone();
        // Set when the app was started from inside a Claude Code session; Claude Code reads it as "nested".
        env.remove("CLAUDECODE");
        let config = AcpAgentConfig::new(npx)
            .args(["-y", kind.package()])
            .envs(env);
        // GHV_ACP_LOG=<file> appends every line in both directions, for debugging an adapter.
        let log = std::env::var_os("GHV_ACP_LOG")
            .and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok())
            .map(Mutex::new);
        let start = std::time::Instant::now();
        let agent = AcpAgent::new(config).with_debug(move |line, direction| {
            if let Some(Ok(mut file)) = log.as_ref().map(Mutex::lock) {
                use std::io::Write as _;
                let _ = writeln!(file, "{:>8.3} {kind:?} {direction:?} {line}", start.elapsed().as_secs_f64());
            }
            if direction == LineDirection::Stderr {
                stderr.push(line);
            }
        });
        Ok(DynConnectTo::new(agent))
    })
}

/// The user's login shell environment, read once. Apps launched from Finder
/// get a minimal PATH, and agents need the user's own to find npx, node,
/// cargo and the like. Falls back to this process's environment.
async fn login_env() -> &'static HashMap<String, String> {
    static ENV: tokio::sync::OnceCell<HashMap<String, String>> = tokio::sync::OnceCell::const_new();
    ENV.get_or_init(async || {
        shell_env()
            .await
            .unwrap_or_else(|| std::env::vars().collect())
    })
    .await
}

/// Runs `$SHELL` as an interactive login shell, as VS Code does, so it reads
/// the same rc files as a terminal (where tools like mise and nvm set PATH).
async fn shell_env() -> Option<HashMap<String, String>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf %s {ENV_MARKER}; /usr/bin/env -0; printf %s {ENV_MARKER}");
    let output = tokio::process::Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    // rc files may exit non-zero after printing a usable environment, so ignore the status.
    let output = tokio::time::timeout(SHELL_ENV_TIMEOUT, output)
        .await
        .ok()?
        .ok()?;
    parse_env(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `env -0` output between two markers. `None` without a PATH, so a
/// broken shell falls back to the process environment.
fn parse_env(output: &str) -> Option<HashMap<String, String>> {
    let (_, rest) = output.split_once(ENV_MARKER)?;
    let (env, _) = rest.split_once(ENV_MARKER)?;
    let env: HashMap<String, String> = env
        .split('\0')
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    env.contains_key("PATH").then_some(env)
}

fn event_from_update(update: acp::SessionUpdate) -> Option<AgentEvent> {
    use acp::SessionUpdate as Update;
    Some(match update {
        Update::AgentMessageChunk(chunk) => AgentEvent::MessageChunk {
            text: block_text(chunk.content)?,
        },
        Update::AgentThoughtChunk(chunk) => AgentEvent::ThoughtChunk {
            text: block_text(chunk.content)?,
        },
        Update::ToolCall(call) => AgentEvent::ToolCall {
            id: call.tool_call_id.to_string(),
            title: call.title,
            kind: wire_name(&call.kind),
            status: call.status.into(),
        },
        Update::ToolCallUpdate(update) => {
            let title = update.fields.title;
            let status = update.fields.status.map(Into::into);
            let text = update.fields.content.and_then(render_content);
            // Claude streams many updates that only grow rawInput, which the UI doesn't show.
            if title.is_none() && status.is_none() && text.is_none() {
                return None;
            }
            AgentEvent::ToolCallUpdate {
                id: update.tool_call_id.to_string(),
                title,
                status,
                text,
            }
        }
        Update::Plan(plan) => AgentEvent::Plan {
            entries: plan
                .entries
                .into_iter()
                .map(|entry| PlanEntry {
                    content: entry.content,
                    status: wire_name(&entry.status),
                })
                .collect(),
        },
        // Commands, modes, usage and the like have no place in the UI yet.
        _ => return None,
    })
}

fn block_text(block: acp::ContentBlock) -> Option<String> {
    match block {
        acp::ContentBlock::Text(text) => Some(text.text),
        acp::ContentBlock::ResourceLink(link) => Some(link.uri),
        _ => None,
    }
}

/// Renders tool output as text; diffs become `-`/`+` lines under the path.
fn render_content(content: Vec<acp::ToolCallContent>) -> Option<String> {
    let parts: Vec<String> = content
        .into_iter()
        .filter_map(|item| match item {
            acp::ToolCallContent::Content(content) => block_text(content.content),
            acp::ToolCallContent::Diff(diff) => {
                let mut text = diff.path.display().to_string();
                for line in diff.old_text.as_deref().unwrap_or_default().lines() {
                    text += &format!("\n-{line}");
                }
                for line in diff.new_text.lines() {
                    text += &format!("\n+{line}");
                }
                Some(text)
            }
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

impl From<acp::ToolCallStatus> for ToolStatus {
    fn from(status: acp::ToolCallStatus) -> Self {
        match status {
            acp::ToolCallStatus::InProgress => Self::InProgress,
            acp::ToolCallStatus::Completed => Self::Completed,
            acp::ToolCallStatus::Failed => Self::Failed,
            _ => Self::Pending,
        }
    }
}

/// The snake_case name ACP uses on the wire, e.g. "end_turn" or "allow_once".
fn wire_name(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{ConnectTo, Error};
    use tokio::sync::mpsc;

    type Events = mpsc::UnboundedReceiver<(String, AgentEvent)>;

    struct Recorder(mpsc::UnboundedSender<(String, AgentEvent)>);

    impl AgentListener for Recorder {
        fn on_event(&self, session_id: String, event: AgentEvent) {
            let _ = self.0.send((session_id, event));
        }
    }

    fn message(text: &str) -> AgentEvent {
        AgentEvent::MessageChunk { text: text.into() }
    }

    /// An in-process ACP agent. Each prompt says "pong", asks for permission,
    /// echoes the chosen option id ("cancelled" if none) and ends the turn.
    /// The prompt "own tool" asks the way Claude does for the app's
    /// draft_comment, "crash" kills the connection and "signed out" fails with
    /// auth_required. Session ids echo the first MCP server, and the cwd
    /// "/signed-out" fails with auth_required.
    fn fake_agent() -> impl ConnectTo<Client> {
        Agent
            .v1()
            .on_receive_request(
                async |request: acp::InitializeRequest, responder, _cx| {
                    let mcp = acp::McpCapabilities::new().http(true);
                    let capabilities = acp::AgentCapabilities::new().mcp_capabilities(mcp);
                    responder.respond(
                        acp::InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(capabilities),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: acp::NewSessionRequest, responder, _cx| {
                    if request.cwd == std::path::Path::new("/signed-out") {
                        return responder.respond_with_error(Error::auth_required());
                    }
                    let id = match request.mcp_servers.first() {
                        Some(acp::McpServer::Http(server)) => {
                            format!(
                                "{} {}={}",
                                server.url, server.headers[0].name, server.headers[0].value
                            )
                        }
                        _ => "session-1".into(),
                    };
                    responder.respond(acp::NewSessionResponse::new(id))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: acp::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    if request.prompt == [acp::ContentBlock::from("crash")] {
                        // A failed task shuts the connection down.
                        return cx.spawn(async { Err(Error::internal_error()) });
                    }
                    if request.prompt == [acp::ContentBlock::from("signed out")] {
                        return responder.respond_with_error(Error::auth_required());
                    }
                    let own_tool = request.prompt == [acp::ContentBlock::from("own tool")];
                    let hang = request.prompt == [acp::ContentBlock::from("hang")];
                    let session_id = request.session_id;
                    let task_cx = cx.clone();
                    cx.spawn(async move {
                        let say = |text: &str| {
                            let chunk = acp::ContentChunk::new(text.into());
                            task_cx.send_notification(acp::SessionNotification::new(
                                session_id.clone(),
                                acp::SessionUpdate::AgentMessageChunk(chunk),
                            ))
                        };
                        say("pong")?;
                        if hang {
                            // Holds the responder and ignores session/cancel, like an agent stuck on a hook.
                            let _responder = responder;
                            return std::future::pending().await;
                        }
                        let mut tool = acp::ToolCallUpdate::new(
                            "tool-1",
                            acp::ToolCallUpdateFields::new().title("Run cargo test".to_string()),
                        );
                        if own_tool {
                            tool = tool.meta(
                                serde_json::from_str::<acp::Meta>(CLAUDE_OWN_TOOL_META).unwrap(),
                            );
                        }
                        let options = vec![
                            acp::PermissionOption::new(
                                "yes",
                                "Allow",
                                acp::PermissionOptionKind::AllowOnce,
                            ),
                            acp::PermissionOption::new(
                                "no",
                                "Reject",
                                acp::PermissionOptionKind::RejectOnce,
                            ),
                        ];
                        let request =
                            acp::RequestPermissionRequest::new(session_id.clone(), tool, options);
                        let answer = task_cx.send_request(request).block_task().await?;
                        let choice = match answer.outcome {
                            acp::RequestPermissionOutcome::Selected(selected) => {
                                selected.option_id.to_string()
                            }
                            _ => "cancelled".into(),
                        };
                        say(&choice)?;
                        responder.respond(acp::PromptResponse::new(acp::StopReason::EndTurn))
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
    }

    fn fake_hub() -> (Arc<AgentHub>, Events) {
        let (sender, events) = mpsc::unbounded_channel();
        let hub = AgentHub::with_launcher(Arc::new(Recorder(sender)), |_, _| {
            Box::pin(async { Ok(DynConnectTo::new(fake_agent())) })
        });
        (Arc::new(hub), events)
    }

    fn start_turn(
        hub: &Arc<AgentHub>,
        session: &str,
    ) -> tokio::task::JoinHandle<Result<String, CoreError>> {
        let (hub, session) = (hub.clone(), session.to_owned());
        tokio::spawn(async move { hub.prompt(session, "ping".into()).await })
    }

    async fn permission_request(events: &mut Events) -> (String, String, Vec<PermissionChoice>) {
        match events.recv().await {
            Some((
                _,
                AgentEvent::PermissionRequest {
                    request_id,
                    title,
                    options,
                    ..
                },
            )) => (request_id, title, options),
            other => panic!("expected a permission request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streams_a_turn_and_round_trips_a_permission() {
        let (hub, mut events) = fake_hub();
        let server = McpServerHttp {
            name: "gh-viewer".into(),
            url: "http://127.0.0.1:1234/mcp".into(),
            headers: vec![HttpHeader {
                name: "Authorization".into(),
                value: "Bearer t".into(),
            }],
        };
        let session = hub
            .new_session(AgentKind::Claude, "/tmp".into(), vec![server])
            .await
            .unwrap();
        assert_eq!(session, "http://127.0.0.1:1234/mcp Authorization=Bearer t");

        let turn = start_turn(&hub, &session);
        assert_eq!(
            events.recv().await,
            Some((session.clone(), message("pong")))
        );
        let (request_id, title, options) = permission_request(&mut events).await;
        assert_eq!(title, "Run cargo test");
        assert_eq!(
            options[0],
            PermissionChoice {
                id: "yes".into(),
                name: "Allow".into(),
                kind: "allow_once".into()
            }
        );
        assert_eq!(options[1].kind, "reject_once");

        hub.respond_permission("unknown".into(), Some("no".into()));
        hub.respond_permission(request_id, Some("yes".into()));
        assert_eq!(events.recv().await, Some((session, message("yes"))));
        assert_eq!(turn.await.unwrap().unwrap(), "end_turn");
    }

    #[tokio::test]
    async fn cancel_answers_open_permission_requests() {
        let (hub, mut events) = fake_hub();
        let session = hub
            .new_session(AgentKind::Claude, "/tmp".into(), vec![])
            .await
            .unwrap();
        let turn = start_turn(&hub, &session);
        assert_eq!(
            events.recv().await,
            Some((session.clone(), message("pong")))
        );
        let (request_id, _, _) = permission_request(&mut events).await;

        hub.cancel(session.clone());
        assert_eq!(events.recv().await, Some((session, message("cancelled"))));
        assert_eq!(turn.await.unwrap().unwrap(), "end_turn");
        // Already answered, so this is a no-op.
        hub.respond_permission(request_id, Some("yes".into()));
    }

    #[tokio::test]
    async fn stop_ends_a_turn_the_agent_never_ends() {
        let (hub, mut events) = fake_hub();
        let session = hub
            .new_session(AgentKind::Codex, "/tmp".into(), vec![])
            .await
            .unwrap();
        let (turn_hub, turn_session) = (hub.clone(), session.clone());
        let turn = tokio::spawn(async move { turn_hub.prompt(turn_session, "hang".into()).await });
        assert_eq!(
            events.recv().await,
            Some((session.clone(), message("pong")))
        );

        hub.cancel(session);
        let stop = tokio::time::timeout(CANCEL_GRACE * 2, turn)
            .await
            .expect("Stop should end the turn after the grace period");
        assert_eq!(stop.unwrap().unwrap(), "cancelled");
    }

    #[tokio::test]
    async fn reports_a_dead_agent_and_respawns_it() {
        let (hub, mut events) = fake_hub();
        let session = hub
            .new_session(AgentKind::Codex, "/tmp".into(), vec![])
            .await
            .unwrap();
        assert!(hub.prompt(session.clone(), "crash".into()).await.is_err());
        match events.recv().await {
            Some((id, AgentEvent::Disconnected { message })) => {
                assert_eq!(id, session);
                assert!(message.starts_with("Codex stopped"), "{message}");
            }
            other => panic!("expected a disconnect, got {other:?}"),
        }
        let error = hub.prompt(session, "ping".into()).await.unwrap_err();
        assert!(error.to_string().contains("session has ended"), "{error}");
        hub.new_session(AgentKind::Codex, "/tmp".into(), vec![])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn explains_how_to_sign_in() {
        let (hub, _events) = fake_hub();
        let error = hub
            .new_session(AgentKind::Codex, "/signed-out".into(), vec![])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Agent error: Codex isn't signed in. Run `codex login` in a terminal, log in, then try again."
        );

        // Claude opens sessions while signed out and fails the first prompt.
        let session = hub
            .new_session(AgentKind::Claude, "/tmp".into(), vec![])
            .await
            .unwrap();
        let error = hub.prompt(session, "signed out".into()).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Agent error: Claude isn't signed in. Run `claude` in a terminal, log in, then try again."
        );
    }

    #[tokio::test]
    async fn allows_own_mcp_tools_without_asking() {
        let (hub, mut events) = fake_hub();
        let session = hub
            .new_session(AgentKind::Claude, "/tmp".into(), vec![])
            .await
            .unwrap();
        let (turn_hub, turn_session) = (hub.clone(), session.clone());
        let turn =
            tokio::spawn(async move { turn_hub.prompt(turn_session, "own tool".into()).await });
        assert_eq!(
            events.recv().await,
            Some((session.clone(), message("pong")))
        );
        // No PermissionRequest in between: the fake agent got "yes", its allow_once option.
        assert_eq!(events.recv().await, Some((session, message("yes"))));
        assert_eq!(turn.await.unwrap().unwrap(), "end_turn");
    }

    // Payloads as claude-agent-acp 0.112.0 and codex-acp 2.12.0 sent them on
    // this machine. Codex auto-reviewed its calls here, so its approval is
    // built from codex-acp's buildMcpPermissionRequest instead.
    const CLAUDE_OWN_TOOL_META: &str = r#"{"claudeCode":{"toolName":"mcp__gh-viewer__draft_comment","mcpServer":{"name":"gh-viewer","source":"dynamic"}}}"#;
    const CLAUDE_REQUEST: &str = r#"{"sessionId":"20ba6720-c7f6-4d44-b54a-4d9a32c1c7b8","toolCall":{"toolCallId":"toolu_01LddYFXH9tbGJTQsru6Y9Xc","kind":"other","status":"pending","title":"gh-viewer: draft_comment","name":"mcp__gh-viewer__draft_comment","content":[],"rawInput":{"path":"src/a.rs","line":1,"side":"RIGHT","body":"hi"},"_meta":{"claudeCode":{"toolName":"mcp__gh-viewer__draft_comment","mcpServer":{"name":"gh-viewer","source":"dynamic"}}}},"options":[{"optionId":"allow-once","name":"Yes","kind":"allow_once"},{"optionId":"reject","name":"No","kind":"reject_once"}]}"#;
    const CODEX_TOOL_CALL: &str = r#"{"sessionId":"01a20fc9-a4ec-7160-8a45-6d7a3ed7f8fe","update":{"sessionUpdate":"tool_call","toolCallId":"exec-d4901259-bafe-42bd-a372-feeb14e2542b","title":"mcp.gh-viewer.draft_comment","kind":"execute","status":"in_progress","rawInput":{"server":"gh-viewer","tool":"draft_comment","arguments":{"path":"src/a.rs","line":1,"side":"RIGHT","body":"hi"}},"_meta":{"is_mcp_tool_call":true}}}"#;
    const CODEX_REQUEST: &str = r#"{"sessionId":"01a20fc9-a4ec-7160-8a45-6d7a3ed7f8fe","toolCall":{"toolCallId":"exec-d4901259-bafe-42bd-a372-feeb14e2542b","kind":"execute","status":"pending"},"_meta":{"is_mcp_tool_approval":true},"options":[{"optionId":"allow_once","name":"Allow","kind":"allow_once"},{"optionId":"allow_always","name":"Always allow","kind":"allow_always"},{"optionId":"cancel","name":"Cancel","kind":"reject_once"}]}"#;

    /// `payload` with each `(pointer, value)` replaced.
    fn edited(payload: &str, edits: &[(&str, serde_json::Value)]) -> serde_json::Value {
        let mut value: serde_json::Value = serde_json::from_str(payload).unwrap();
        for (pointer, replacement) in edits {
            *value.pointer_mut(pointer).unwrap() = replacement.clone();
        }
        value
    }

    #[test]
    fn matches_own_mcp_tools_as_claude_sends_them() {
        let allow =
            |request: &serde_json::Value, kind| auto_allow(kind, request, &mut HashSet::new());
        let request = edited(CLAUDE_REQUEST, &[]);
        assert_eq!(
            allow(&request, AgentKind::Claude).as_deref(),
            Some("allow-once")
        );
        // The Codex rule doesn't apply to Claude's payloads, nor the reverse.
        assert_eq!(allow(&request, AgentKind::Codex), None);

        let meta = "/toolCall/_meta/claudeCode";
        let from_user_config = edited(
            CLAUDE_REQUEST,
            &[(&format!("{meta}/mcpServer/source"), "user".into())],
        );
        let other_server = edited(
            CLAUDE_REQUEST,
            &[
                (&format!("{meta}/mcpServer/name"), "other".into()),
                (
                    &format!("{meta}/toolName"),
                    "mcp__other__draft_comment".into(),
                ),
            ],
        );
        let other_tool = edited(
            CLAUDE_REQUEST,
            &[(&format!("{meta}/toolName"), "mcp__gh-viewer__merge".into())],
        );
        let bash = edited(
            CLAUDE_REQUEST,
            &[("/toolCall/_meta", serde_json::json!({}))],
        );
        let no_allow_once = edited(
            CLAUDE_REQUEST,
            &[("/options/0/kind", "allow_always".into())],
        );
        for request in [
            from_user_config,
            other_server,
            other_tool,
            bash,
            no_allow_once,
        ] {
            assert_eq!(allow(&request, AgentKind::Claude), None, "{request}");
        }
    }

    #[test]
    fn matches_own_mcp_tools_as_codex_sends_them() {
        let notification: acp::SessionNotification = serde_json::from_str(CODEX_TOOL_CALL).unwrap();
        let session_id = notification.session_id.to_string();
        let request = edited(CODEX_REQUEST, &[]);
        let mut calls = HashSet::new();

        // Unknown until the tool call is announced, then allowed once.
        assert_eq!(auto_allow(AgentKind::Codex, &request, &mut calls), None);
        track_own_codex_call(&mut calls, &session_id, &notification.update);
        assert_eq!(
            auto_allow(AgentKind::Codex, &request, &mut calls).as_deref(),
            Some("allow_once")
        );
        assert_eq!(auto_allow(AgentKind::Codex, &request, &mut calls), None);

        // Another server's tool of the same name isn't tracked.
        let other: acp::SessionNotification = serde_json::from_value(edited(
            CODEX_TOOL_CALL,
            &[("/update/rawInput/server", "other".into())],
        ))
        .unwrap();
        track_own_codex_call(&mut calls, &session_id, &other.update);
        assert!(calls.is_empty());

        // A finished call is forgotten.
        track_own_codex_call(&mut calls, &session_id, &notification.update);
        let done = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            "exec-d4901259-bafe-42bd-a372-feeb14e2542b",
            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Completed),
        ));
        track_own_codex_call(&mut calls, &session_id, &done);
        assert!(calls.is_empty());

        // Only an MCP tool approval for a tracked call qualifies.
        track_own_codex_call(&mut calls, &session_id, &notification.update);
        let command = edited(CODEX_REQUEST, &[("/_meta", serde_json::json!({}))]);
        assert_eq!(auto_allow(AgentKind::Codex, &command, &mut calls), None);
    }

    #[test]
    fn parses_shell_env_between_markers() {
        let output = format!(
            "Welcome to fish\n{ENV_MARKER}PATH=/opt/bin:/usr/bin\0MULTI=one\ntwo\0EMPTY=\0EQ=a=b\0{ENV_MARKER}bye\n"
        );
        let env = parse_env(&output).unwrap();
        assert_eq!(env["PATH"], "/opt/bin:/usr/bin");
        assert_eq!(env["MULTI"], "one\ntwo");
        assert_eq!(env["EMPTY"], "");
        assert_eq!(env["EQ"], "a=b");
        assert_eq!(env.len(), 4);

        assert_eq!(parse_env("PATH=/usr/bin"), None);
        assert_eq!(
            parse_env(&format!("{ENV_MARKER}HOME=/x\0{ENV_MARKER}")),
            None
        );
    }

    #[test]
    fn renders_tool_content_as_text() {
        let diff = acp::Diff::new("src/a.rs", "new\nlines").old_text("old".to_string());
        let content = vec![
            acp::ToolCallContent::from("ran"),
            acp::ToolCallContent::Diff(diff),
        ];
        assert_eq!(
            render_content(content).as_deref(),
            Some("ran\nsrc/a.rs\n-old\n+new\n+lines")
        );
        assert_eq!(render_content(vec![]), None);
    }

    #[test]
    fn appends_codex_command_output_chunks() {
        // As codex-acp 2.12 sends them: the output in `_meta`, the exit with the last chunk.
        let chunk = |data: &str, status: Option<&str>| {
            let mut json = serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "exec-1",
                "_meta": {"terminal_output_delta": {"data": data, "terminal_id": "exec-1"}},
            });
            if let Some(status) = status {
                json["status"] = status.into();
            }
            serde_json::from_value::<acp::SessionUpdate>(json).unwrap()
        };
        let text = |update| match event_from_update(update) {
            Some(AgentEvent::ToolCallUpdate { text, .. }) => text,
            other => panic!("expected a tool call update, got {other:?}"),
        };
        let mut outputs = HashMap::new();

        let mut first = chunk("README.md\n", None);
        collect_codex_output(&mut outputs, "s", &mut first);
        assert_eq!(text(first).as_deref(), Some("README.md\n"));

        let mut last = chunk("src\n", Some("completed"));
        collect_codex_output(&mut outputs, "s", &mut last);
        assert_eq!(text(last).as_deref(), Some("README.md\nsrc\n"));
        assert!(outputs.is_empty(), "a finished command's output is dropped");
    }

    /// Checks the Finder case: with a stripped environment, the login shell
    /// still yields a PATH that finds npx. Run as
    /// `env -i HOME=$HOME SHELL=$SHELL <test binary> --ignored finds_npx`.
    #[tokio::test]
    #[ignore = "depends on this machine's login shell"]
    async fn finds_npx_through_the_login_shell() {
        let env = shell_env()
            .await
            .expect("login shell should print its environment");
        let path = &env["PATH"];
        assert!(
            std::env::split_paths(path).any(|dir| dir.join("npx").is_file()),
            "{path}"
        );
    }

    /// Talks to the real adapter with the machine owner's signed-in agent.
    async fn replies_pong(kind: AgentKind) {
        let dir = std::env::temp_dir().join(format!("ghv-agent-{kind:?}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = std::process::Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status();
        assert!(git.unwrap().success());

        let (sender, mut events) = mpsc::unbounded_channel();
        let hub = AgentHub::new(Arc::new(Recorder(sender)));
        let session = hub
            .new_session(kind, dir.to_string_lossy().into(), vec![])
            .await
            .unwrap();
        let prompt = "Reply with exactly the word: pong. Do not use any tools.";
        let stop_reason = hub.prompt(session, prompt.into()).await.unwrap();

        let mut chunks = Vec::new();
        while let Ok((_, event)) = events.try_recv() {
            eprintln!("{event:?}");
            if let AgentEvent::MessageChunk { text } = event {
                chunks.push(text);
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(stop_reason, "end_turn");
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.to_lowercase().contains("pong")),
            "{chunks:?}"
        );
    }

    /// Opens a session in a repo whose committed agent config (Claude hooks, a
    /// Codex stdio MCP server) touches a marker, as a PR's could. True if it ran.
    /// `hide` applies the worktree rule from `checkout::hide_agent_config` first.
    async fn runs_project_config(kind: AgentKind, hide: bool) -> bool {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, marker) = (tmp.path().join("repo"), tmp.path().join("marker"));
        let marker = marker.to_str().unwrap();
        let touch = serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("touch {marker}") }] }]);
        let claude = serde_json::json!({ "hooks": { "SessionStart": touch, "UserPromptSubmit": touch } });
        let codex = format!("[mcp_servers.probe]\ncommand = \"/usr/bin/touch\"\nargs = [\"{marker}\"]\n");
        for dir in [".claude", ".codex"] {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
        }
        std::fs::write(repo.join(".claude/settings.json"), claude.to_string()).unwrap();
        std::fs::write(repo.join(".codex/config.toml"), codex).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("/usr/bin/git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&repo)
                .status();
            assert!(status.unwrap().success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "config"]);
        if hide {
            crate::checkout::hide_agent_config(repo.to_str().unwrap()).await.unwrap();
        }

        let (sender, _events) = mpsc::unbounded_channel();
        let hub = AgentHub::new(Arc::new(Recorder(sender)));
        let session = hub
            .new_session(kind, repo.to_string_lossy().into(), vec![])
            .await
            .unwrap();
        let stop = hub.prompt(session, "Reply with exactly: pong".into()).await.unwrap();
        assert_eq!(stop, "end_turn");
        std::path::Path::new(marker).exists()
    }

    #[tokio::test]
    #[ignore = "spawns the real Claude adapter and spends tokens"]
    async fn claude_ignores_the_projects_settings() {
        assert!(!runs_project_config(AgentKind::Claude, false).await);
    }

    #[tokio::test]
    #[ignore = "spawns the real Codex adapter and spends tokens"]
    async fn codex_ignores_a_hidden_projects_config() {
        assert!(!runs_project_config(AgentKind::Codex, true).await);
    }

    #[tokio::test]
    #[ignore = "spawns the real Claude adapter and spends tokens"]
    async fn claude_replies_pong() {
        replies_pong(AgentKind::Claude).await;
    }

    #[tokio::test]
    #[ignore = "spawns the real Codex adapter and spends tokens"]
    async fn codex_replies_pong() {
        replies_pong(AgentKind::Codex).await;
    }
}

//! Localhost MCP server that gives agents review tools such as `draft_comment`.
//!
//! One server on one port serves every agent session. Each registration gets
//! its own path (`/mcp/<id>`) and bearer token, which map to that session's PR.
//! The tools never touch GitHub: `draft_comment` only adds to local drafts.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, HeaderValue, WWW_AUTHENTICATE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::{CoreError, DiffSide, runtime};

/// What one agent session's tools can see and do, for one PR.
pub(crate) trait ReviewTools: Send + Sync + 'static {
    /// JSON: repo, number, title, body, base/head refs and OIDs, changed files.
    fn pr_context(&self) -> serde_json::Value;
    /// JSON: existing review threads, optionally only for `path`.
    fn review_threads(&self, path: Option<String>) -> serde_json::Value;
    /// Adds a local draft. Ok(text for the agent) or Err(reason the agent should fix, e.g. line not in diff).
    fn draft_comment(&self, args: DraftCommentArgs) -> Result<String, String>;
}

/// Arguments of the `draft_comment` tool. The field docs become the tool's
/// input schema, which is all the agent sees, so they're written for it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DraftCommentArgs {
    /// File path relative to the repository root, as it appears in the diff.
    pub path: String,
    /// The line to comment on, or the last line of a multi-line comment. A
    /// new-file line number when `side` is RIGHT, an old-file line number when
    /// `side` is LEFT.
    #[schemars(range(min = 1))]
    pub line: u32,
    /// Which side of the diff `line` is on. RIGHT for added lines and unchanged
    /// context lines, using new-file line numbers. LEFT for removed lines, using
    /// old-file line numbers.
    pub side: DiffSide,
    /// For a multi-line comment, the first line of the range. Pass `start_side`
    /// with it. Omit both for a single-line comment.
    #[schemars(range(min = 1))]
    pub start_line: Option<u32>,
    /// Which side of the diff `start_line` is on, with the same meaning as `side`.
    pub start_side: Option<DiffSide>,
    /// The comment in GitHub-flavored markdown. Cover one issue per comment.
    /// To propose a concrete fix on RIGHT-side lines, include a ```suggestion
    /// fenced block holding the full replacement for exactly the commented
    /// lines; GitHub lets the author apply it with one click. Suggestions
    /// don't work on LEFT-side lines.
    pub body: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ListReviewThreadsArgs {
    /// Only return threads on this file, as a path relative to the repository
    /// root. Omit to get threads on every file.
    path: Option<String>,
}

pub(crate) struct McpServer {
    /// `http://127.0.0.1:<port>/mcp/`; a registration's URL is this plus its id.
    base_url: String,
    registrations: Mutex<HashMap<String, Registration>>,
}

/// Where one agent session reaches its tools.
#[allow(dead_code)] // `bearer_token` is read once integration passes it to the agent
pub(crate) struct McpEndpoint {
    pub url: String,
    pub bearer_token: String,
}

#[derive(Clone)]
struct Registration {
    token: String,
    service: StreamableHttpService<Tools, NeverSessionManager>,
}

#[allow(dead_code)] // used once integration wires agent sessions to drafts
impl McpServer {
    /// Binds 127.0.0.1 on a random port and serves on crate::runtime().
    pub(crate) async fn start() -> Result<Arc<McpServer>, CoreError> {
        let error = |e: std::io::Error| {
            CoreError::Agent(format!(
                "couldn't start the local MCP server for agent tools: {e}"
            ))
        };
        // Bind with std and register the socket with crate::runtime(), so it
        // keeps working after the caller's runtime (e.g. a test's) is gone.
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(error)?;
        listener.set_nonblocking(true).map_err(error)?;
        let base_url = format!("http://{}/mcp/", listener.local_addr().map_err(error)?);
        let listener = {
            let _runtime = runtime().enter();
            tokio::net::TcpListener::from_std(listener).map_err(error)?
        };
        let server = Arc::new(McpServer {
            base_url,
            registrations: Mutex::default(),
        });
        runtime().spawn(server.clone().serve(listener));
        Ok(server)
    }

    /// Gives a session its own URL and token.
    pub(crate) fn register(&self, tools: Arc<dyn ReviewTools>) -> McpEndpoint {
        let id = random_hex();
        let token = random_hex();
        let service = StreamableHttpService::new(
            move || Ok(Tools(tools.clone())),
            Default::default(),
            StreamableHttpServerConfig::default()
                // Every tool is a quick request and reply, so there's no
                // session or event stream worth keeping.
                .with_legacy_session_mode(false)
                .with_json_response(true)
                .with_sse_keep_alive(None)
                // The MCP spec requires this against DNS rebinding. Agents send
                // no Origin, so this only ever stops browsers.
                .with_allowed_origins(["http", "https"].into_iter().flat_map(|scheme| {
                    ["localhost", "127.0.0.1", "[::1]"].map(|host| format!("{scheme}://{host}:*"))
                })),
        );
        self.registrations
            .lock()
            .expect("MCP registrations lock poisoned")
            .insert(
                id.clone(),
                Registration {
                    token: token.clone(),
                    service,
                },
            );
        McpEndpoint {
            url: format!("{}{id}", self.base_url),
            bearer_token: token,
        }
    }

    /// Revokes a registration; later requests to it get 404.
    pub(crate) fn unregister(&self, endpoint: &McpEndpoint) {
        if let Some(id) = endpoint.url.strip_prefix(&self.base_url) {
            self.registrations
                .lock()
                .expect("MCP registrations lock poisoned")
                .remove(id);
        }
    }

    async fn serve(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(_) => {
                    // Typically out of file descriptors; back off instead of spinning.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let server = self.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let server = server.clone();
                    async move { Ok::<_, Infallible>(server.handle(request).await) }
                });
                // An error here means the client hung up; there's no one to tell.
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<BoxBody<Bytes, Infallible>> {
        let registration = request.uri().path().strip_prefix("/mcp/").and_then(|id| {
            let registrations = self
                .registrations
                .lock()
                .expect("MCP registrations lock poisoned");
            registrations.get(id).cloned()
        });
        let Some(registration) = registration else {
            return plain_response(
                StatusCode::NOT_FOUND,
                "No MCP endpoint at this URL. Its agent session has ended or never existed.",
            );
        };
        let token = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if !token
            .is_some_and(|token| constant_time_eq(token.as_bytes(), registration.token.as_bytes()))
        {
            let mut response = plain_response(
                StatusCode::UNAUTHORIZED,
                "Missing or wrong bearer token. Send the token GH Viewer gave this session as `Authorization: Bearer <token>`.",
            );
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return response;
        }
        // rmcp checks Host and Origin, then speaks MCP.
        registration.service.handle(request).await
    }
}

/// The MCP tools of one registration.
struct Tools(Arc<dyn ReviewTools>);

#[tool_router]
impl Tools {
    #[tool(
        description = "Get the pull request under review as JSON: repository, number, title, body, base and head refs and commit OIDs, and the changed files. Your working directory is a checkout of the PR's head commit. Call this first.",
        annotations(read_only_hint = true)
    )]
    fn get_pr_context(&self) -> String {
        self.0.pr_context().to_string()
    }

    #[tool(
        description = "List the existing review threads on this pull request as JSON, optionally only those on one file. Check them before drafting so you don't repeat a point that's already been made.",
        annotations(read_only_hint = true)
    )]
    fn list_review_threads(&self, Parameters(args): Parameters<ListReviewThreadsArgs>) -> String {
        self.0.review_threads(args.path).to_string()
    }

    #[tool(
        description = "Draft a review comment on lines of this pull request's diff. Drafts are only suggestions: the human reviewer edits, discards or submits them, and nothing is posted to GitHub until they submit. Make one comment per distinct issue and write it in GitHub-flavored markdown. The comment must target lines inside the diff's hunks (changed lines or the context lines shown around them). For a multi-line comment, pass `start_line` and `start_side` for the first line, and `line` and `side` for the last. If the draft is rejected, the error says what to fix; fix the arguments and call this again."
    )]
    fn draft_comment(
        &self,
        Parameters(args): Parameters<DraftCommentArgs>,
    ) -> Result<String, String> {
        // rmcp turns Err into a tool result with isError, which the agent sees and can act on.
        self.0.draft_comment(args)
    }
}

#[tool_handler(
    name = "gh-viewer",
    instructions = "Review tools for the pull request checked out in your working directory. Start with get_pr_context. Use draft_comment to suggest review comments; they stay local until the human reviewer edits and submits them."
)]
impl ServerHandler for Tools {}

fn plain_response(
    status: StatusCode,
    message: &'static str,
) -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(Full::new(Bytes::from_static(message.as_bytes())).boxed());
    *response.status_mut() = status;
    response
}

/// 128 bits from the OS's CSPRNG, as hex.
pub(crate) fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS random number generator should be available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Doesn't stop at the first mismatch, so response timing can't reveal how
/// much of a guessed token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Records calls and rejects comments on line 0, like a diff check would.
    #[derive(Default)]
    struct FakeTools {
        drafts: Mutex<Vec<DraftCommentArgs>>,
        thread_paths: Mutex<Vec<Option<String>>>,
    }

    impl ReviewTools for FakeTools {
        fn pr_context(&self) -> Value {
            json!({ "repo": "o/r", "number": 7, "title": "Fix bug" })
        }
        fn review_threads(&self, path: Option<String>) -> Value {
            self.thread_paths.lock().unwrap().push(path);
            json!([])
        }
        fn draft_comment(&self, args: DraftCommentArgs) -> Result<String, String> {
            if args.line == 0 {
                return Err("Line 0 of src/a.rs is not in the diff.".into());
            }
            self.drafts.lock().unwrap().push(args);
            Ok("Draft added.".into())
        }
    }

    async fn serve_fake() -> (Arc<McpServer>, Arc<FakeTools>, McpEndpoint) {
        let server = McpServer::start().await.unwrap();
        let tools = Arc::new(FakeTools::default());
        let endpoint = server.register(tools.clone());
        (server, tools, endpoint)
    }

    fn post(endpoint: &McpEndpoint, body: &Value) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(&endpoint.url)
            .header("Accept", "application/json, text/event-stream")
            .json(body)
    }

    /// Sends one authorized JSON-RPC request and returns its `result`.
    async fn call(endpoint: &McpEndpoint, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = post(endpoint, &body)
            .bearer_auth(&endpoint.bearer_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let reply: Value = response.json().await.unwrap();
        assert!(reply["error"].is_null(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    async fn call_tool(endpoint: &McpEndpoint, name: &str, arguments: Value) -> Value {
        call(
            endpoint,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    #[tokio::test]
    async fn speaks_mcp_over_http() {
        let (_server, tools, endpoint) = serve_fake().await;
        assert!(endpoint.url.starts_with("http://127.0.0.1:"));

        let init = call(
            &endpoint,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" }
            }),
        )
        .await;
        assert_eq!(init["serverInfo"]["name"], "gh-viewer");
        assert!(init["capabilities"]["tools"].is_object());

        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let response = post(&endpoint, &initialized)
            .bearer_auth(&endpoint.bearer_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 202);

        let list = call(&endpoint, "tools/list", json!({})).await;
        let schema = |name: &str| {
            let tool = list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name);
            tool.unwrap_or_else(|| panic!("{name} missing from {list}"))["inputSchema"].clone()
        };
        for name in ["get_pr_context", "list_review_threads", "draft_comment"] {
            assert_eq!(schema(name)["type"], "object", "{name}");
        }
        let draft = schema("draft_comment");
        let mut required: Vec<&str> = draft["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        required.sort();
        assert_eq!(required, ["body", "line", "path", "side"]);
        // Inlined, so clients that don't follow `$ref` still see the values.
        assert_eq!(
            draft["properties"]["side"]["enum"],
            json!(["LEFT", "RIGHT"])
        );
        assert_eq!(draft["properties"]["line"]["minimum"], 1);

        let drafted = call_tool(
            &endpoint,
            "draft_comment",
            json!({
                "path": "src/a.rs", "line": 12, "side": "RIGHT",
                "start_line": 10, "start_side": "LEFT", "body": "Off by one?"
            }),
        )
        .await;
        assert_ne!(drafted["isError"], true);
        assert_eq!(drafted["content"][0]["text"], "Draft added.");
        assert_eq!(
            *tools.drafts.lock().unwrap(),
            [DraftCommentArgs {
                path: "src/a.rs".into(),
                line: 12,
                side: DiffSide::Right,
                start_line: Some(10),
                start_side: Some(DiffSide::Left),
                body: "Off by one?".into(),
            }]
        );

        let context = call_tool(&endpoint, "get_pr_context", json!({})).await;
        let text = context["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            tools.pr_context()
        );

        call_tool(&endpoint, "list_review_threads", json!({})).await;
        call_tool(
            &endpoint,
            "list_review_threads",
            json!({ "path": "src/a.rs" }),
        )
        .await;
        assert_eq!(
            *tools.thread_paths.lock().unwrap(),
            [None, Some("src/a.rs".into())]
        );
    }

    #[tokio::test]
    async fn rejected_drafts_come_back_as_tool_errors() {
        let (_server, tools, endpoint) = serve_fake().await;
        let result = call_tool(
            &endpoint,
            "draft_comment",
            json!({ "path": "src/a.rs", "line": 0, "side": "RIGHT", "body": "Hm" }),
        )
        .await;
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["content"][0]["text"],
            "Line 0 of src/a.rs is not in the diff."
        );
        assert!(tools.drafts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_tokens_origins_and_endpoints() {
        let (server, _tools, endpoint) = serve_fake().await;
        let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
        let status =
            async |request: reqwest::RequestBuilder| request.send().await.unwrap().status();
        let other = server.register(Arc::new(FakeTools::default()));

        assert_eq!(status(post(&endpoint, &ping)).await, 401);
        assert_eq!(
            status(post(&endpoint, &ping).bearer_auth("wrong")).await,
            401
        );
        assert_eq!(
            status(post(&endpoint, &ping).bearer_auth(&other.bearer_token)).await,
            401
        );

        let authorized = || post(&endpoint, &ping).bearer_auth(&endpoint.bearer_token);
        assert_eq!(status(authorized()).await, 200);
        assert_eq!(
            status(authorized().header("Origin", "http://localhost:3000")).await,
            200
        );
        assert_eq!(
            status(authorized().header("Origin", "https://evil.example")).await,
            403
        );
        assert_eq!(
            status(authorized().header("Origin", "http://127.0.0.1.evil.example")).await,
            403
        );

        server.unregister(&endpoint);
        assert_eq!(status(authorized()).await, 404);
        let unknown = format!("{}x", other.url);
        let unknown = reqwest::Client::new()
            .post(unknown)
            .bearer_auth(&other.bearer_token);
        assert_eq!(status(unknown.json(&ping)).await, 404);
        // Revoking one session leaves the others working.
        assert_eq!(
            status(post(&other, &ping).bearer_auth(&other.bearer_token)).await,
            200
        );
    }

    #[test]
    fn tokens_are_128_random_bits_compared_in_constant_time() {
        let (a, b) = (random_hex(), random_hex());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(constant_time_eq(a.as_bytes(), a.as_bytes()));
        assert!(!constant_time_eq(a.as_bytes(), b.as_bytes()));
        assert!(!constant_time_eq(a.as_bytes(), &a.as_bytes()[..31]));
    }
}

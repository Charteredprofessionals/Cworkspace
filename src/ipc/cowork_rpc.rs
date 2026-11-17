use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, oneshot};

pub const PIPE_NAME: &str = r"\\.\pipe\jcode-cowork";

/// Maximum accepted JSON-RPC message size (S-7): a connected client must not
/// be able to exhaust daemon memory with a single oversized line or frame.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Local file holding the daemon's capability token. The desktop GUI reads it
/// and authenticates every request. Without a valid token the daemon serves no
/// method and delivers no event, so a same-user local process cannot drive the
/// agent or self-approve its own actions (S-1).
pub const TOKEN_FILE_NAME: &str = "jcode-cowork-ipc.token";

pub fn token_file_path() -> PathBuf {
    std::env::temp_dir().join(TOKEN_FILE_NAME)
}

/// 256-bit capability token (four independent CSPRNG words, hex-encoded).
fn generate_capability_token() -> String {
    let a = rand::random::<u64>();
    let b = rand::random::<u64>();
    let c = rand::random::<u64>();
    let d = rand::random::<u64>();
    format!("{a:016x}{b:016x}{c:016x}{d:016x}")
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |diff, (x, y)| diff | (x ^ y))
        == 0
}

static GLOBAL_RPC: OnceLock<CoworkRpc> = OnceLock::new();

pub fn global() -> &'static CoworkRpc {
    GLOBAL_RPC.get_or_init(CoworkRpc::new)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum CoworkEvent {
    AgentSpawned {
        agent_id: String,
        task_id: String,
    },
    ToolInvoked {
        tool_call_id: String,
        tool_name: String,
    },
    ArtifactCreated {
        path: String,
        mime_type: String,
    },
    ApprovalRequired {
        id: String,
        tool_name: String,
        description: String,
    },
    TaskCompleted {
        task_id: String,
        success: bool,
        summary: Option<String>,
    },
}

#[derive(Clone)]
pub struct CoworkRpc {
    inner: Arc<CoworkRpcInner>,
}

struct CoworkRpcInner {
    auth_token: String,
    events: broadcast::Sender<CoworkEvent>,
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    mounted_roots: Mutex<HashMap<String, Vec<PathBuf>>>,
    jail_registrations: Mutex<HashMap<String, crate::tool::SessionPathValidatorRegistration>>,
    approval_registrations: Mutex<HashMap<String, crate::tool::SessionApprovalRegistration>>,
    tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl CoworkRpc {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(CoworkRpcInner {
                auth_token: generate_capability_token(),
                events,
                approvals: Mutex::new(HashMap::new()),
                mounted_roots: Mutex::new(HashMap::new()),
                jail_registrations: Mutex::new(HashMap::new()),
                approval_registrations: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// True when the request carries this daemon's capability token. Every
    /// method requires it (S-1): only token holders may invoke commands or
    /// resolve approvals, so broadcast approval IDs are harmless - an
    /// unauthenticated listener can see nothing and do nothing.
    pub fn authorize(&self, request: &RpcRequest) -> bool {
        request
            .auth
            .as_ref()
            .is_some_and(|auth| constant_time_eq(&auth.token, &self.inner.auth_token))
    }

    /// Write the capability token to the local token file so the desktop GUI
    /// can authenticate. The file is created owner-only (0600 on Unix; the
    /// Windows per-user temp directory is ACL-protected) and the daemon
    /// publishes it before the pipe accepts its first client, so a client can
    /// never see the pipe without the token already being in place.
    pub async fn publish_capability_token(&self) -> Result<PathBuf> {
        let path = token_file_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create directory for {}", path.display())
            })?;
        }
        #[cfg(unix)]
        {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .await
                .with_context(|| format!("failed to create {}", path.display()))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, self.inner.auth_token.as_bytes())
                .await?;
            tokio::io::AsyncWriteExt::flush(&mut file).await?;
        }
        #[cfg(not(unix))]
        tokio::fs::write(&path, self.inner.auth_token.as_bytes())
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    pub async fn register_agent_session(
        &self,
        session_id: &str,
        roots: Vec<PathBuf>,
    ) -> Result<(
        crate::tool::SessionPathValidatorRegistration,
        crate::tool::SessionApprovalRegistration,
        crate::tool::SessionArtifactCreatedRegistration,
    )> {
        let jail_roots = validate_workspace_roots(roots).await?;
        let session_id = session_id.to_owned();
        let registration_session_id = session_id.clone();
        let path_registration = tokio::task::spawn_blocking(move || {
            crate::sandbox::register_session_jail(registration_session_id, jail_roots)
        })
        .await
        .context("failed to initialize Cowork path jail")??;
        let weak = Arc::downgrade(&self.inner);
        let approval_registration = crate::tool::register_session_approval_requester(
            session_id.clone(),
            move |id, tool_name, description| {
                let weak = weak.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .request_tool_approval(id, tool_name, description)
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        );
        let weak = Arc::downgrade(&self.inner);
        let artifact_registration =
            crate::tool::register_session_artifact_created_notifier(session_id, move |artifact| {
                let weak = weak.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .emit_event(CoworkEvent::ArtifactCreated {
                            path: artifact.path.display().to_string(),
                            mime_type: artifact.mime_type,
                        })
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                }
            });
        Ok((
            path_registration,
            approval_registration,
            artifact_registration,
        ))
    }

    pub async fn start_task(&self, goal: String, allowed_roots: Vec<PathBuf>) -> Result<String> {
        self.start_task_with(
            goal,
            allowed_roots,
            |goal, roots, task_id, rpc| async move {
                crate::cli::commands::run_cowork_task(goal, roots, task_id, rpc).await
            },
        )
        .await
    }

    async fn start_task_with<F, Fut>(
        &self,
        goal: String,
        allowed_roots: Vec<PathBuf>,
        run: F,
    ) -> Result<String>
    where
        F: FnOnce(String, Vec<PathBuf>, String, CoworkRpc) -> Fut,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        anyhow::ensure!(!goal.trim().is_empty(), "task goal cannot be empty");
        let allowed_roots = validate_workspace_roots(allowed_roots).await?;
        let task_id = crate::id::new_id("cowork");
        let (start, started) = oneshot::channel();
        let rpc = self.clone();
        let run_task_id = task_id.clone();
        let work = run(goal, allowed_roots, run_task_id.clone(), rpc.clone());
        let handle = tokio::spawn(async move {
            if started.await.is_err() {
                return;
            }
            let result = work.await;
            rpc.inner.tasks.lock().await.remove(&run_task_id);
            let (success, summary) = match result {
                Ok(summary) => (true, Some(summary)),
                Err(error) => (false, Some(error.to_string())),
            };
            let _ = rpc.emit_event(CoworkEvent::TaskCompleted {
                task_id: run_task_id,
                success,
                summary,
            });
        });
        self.inner
            .tasks
            .lock()
            .await
            .insert(task_id.clone(), handle);
        let _ = start.send(());
        Ok(task_id)
    }

    pub async fn stop_task(&self, task_id: &str) -> bool {
        let handle = self.inner.tasks.lock().await.remove(task_id);
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
            self.inner
                .approvals
                .lock()
                .await
                .retain(|_, sender| !sender.is_closed());
            let _ = self.emit_event(CoworkEvent::TaskCompleted {
                task_id: task_id.to_owned(),
                success: false,
                summary: Some("Task stopped by user".to_owned()),
            });
            true
        } else {
            false
        }
    }

    pub async fn mount_folder(&self, session_id: &str, path: &Path) -> Result<PathBuf> {
        let canonical = tokio::fs::canonicalize(path)
            .await
            .with_context(|| format!("cannot resolve mounted folder {}", path.display()))?;
        if !tokio::fs::metadata(&canonical).await?.is_dir() {
            return Err(anyhow!("mounted workspace root is not a directory"));
        }

        let roots = {
            let mounted = self.inner.mounted_roots.lock().await;
            let mut roots = mounted.get(session_id).cloned().unwrap_or_default();
            if !roots.contains(&canonical) {
                roots.push(canonical.clone());
            }
            roots
        };
        self.set_workspace_roots(session_id, roots).await?;
        Ok(canonical)
    }

    pub async fn unmount_folder(&self, session_id: &str, path: &Path) -> Result<bool> {
        let canonical = tokio::fs::canonicalize(path)
            .await
            .with_context(|| format!("cannot resolve mounted folder {}", path.display()))?;
        let mut roots = self
            .inner
            .mounted_roots
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let before = roots.len();
        roots.retain(|root| root != &canonical);
        if roots.len() == before {
            return Ok(false);
        }
        self.set_workspace_roots(session_id, roots).await?;
        Ok(true)
    }

    async fn set_workspace_roots(&self, session_id: &str, roots: Vec<PathBuf>) -> Result<()> {
        let roots = validate_workspace_roots(roots).await?;
        let jail_roots = roots.clone();
        let jail = tokio::task::spawn_blocking(move || {
            crate::sandbox::path_jail::PathJail::new(jail_roots)
        })
        .await
        .context("failed to initialize Cowork path jail")??;
        let registration =
            crate::tool::register_session_path_validator(session_id.to_owned(), move |path| {
                jail.verify_path(path)
            });
        let weak: Weak<CoworkRpcInner> = Arc::downgrade(&self.inner);
        let approval_registration = crate::tool::register_session_approval_requester(
            session_id.to_owned(),
            move |id, tool_name, description| {
                let weak = weak.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .request_tool_approval(id, tool_name, description)
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        );
        self.inner
            .mounted_roots
            .lock()
            .await
            .insert(session_id.to_owned(), roots);
        self.inner
            .jail_registrations
            .lock()
            .await
            .insert(session_id.to_owned(), registration);
        self.inner
            .approval_registrations
            .lock()
            .await
            .insert(session_id.to_owned(), approval_registration);
        Ok(())
    }

    pub fn emit_event(&self, event: CoworkEvent) -> Result<usize> {
        self.inner
            .events
            .send(event)
            .context("no Cowork IPC client is connected")
    }

    pub async fn request_tool_approval(
        &self,
        id: impl Into<String>,
        tool_name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<bool> {
        if self.inner.events.receiver_count() == 0 {
            return Err(anyhow!(
                "no Cowork IPC client is connected to approve this action"
            ));
        }

        let id = id.into();
        let (sender, receiver) = oneshot::channel();
        {
            let mut approvals = self.inner.approvals.lock().await;
            if approvals.contains_key(&id) {
                return Err(anyhow!("approval request id is already pending: {id}"));
            }
            approvals.insert(id.clone(), sender);
        }

        if self
            .emit_event(CoworkEvent::ApprovalRequired {
                id: id.clone(),
                tool_name: tool_name.into(),
                description: description.into(),
            })
            .is_err()
        {
            self.inner.approvals.lock().await.remove(&id);
            return Err(anyhow!(
                "Cowork IPC client disconnected before approval was requested"
            ));
        }

        receiver
            .await
            .context("Cowork IPC client disconnected before approving or rejecting")
    }

    pub async fn approve_tool_call(&self, id: &str, approved: bool) -> bool {
        self.inner
            .approvals
            .lock()
            .await
            .remove(id)
            .is_some_and(|sender| sender.send(approved).is_ok())
    }

    fn subscribe(&self) -> broadcast::Receiver<CoworkEvent> {
        self.inner.events.subscribe()
    }
}

pub(crate) async fn validate_workspace_roots(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    anyhow::ensure!(
        !roots.is_empty(),
        "Cowork tasks require at least one mounted workspace root"
    );
    let mut canonical_roots = Vec::with_capacity(roots.len());
    for root in roots {
        let canonical = tokio::fs::canonicalize(&root)
            .await
            .with_context(|| format!("cannot resolve Cowork workspace root {}", root.display()))?;
        if !tokio::fs::metadata(&canonical).await?.is_dir() {
            return Err(anyhow!(
                "Cowork workspace root is not a directory: {}",
                root.display()
            ));
        }
        if !canonical_roots.contains(&canonical) {
            canonical_roots.push(canonical);
        }
    }
    Ok(canonical_roots)
}

impl Default for CoworkRpc {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct RpcAuth {
    token: String,
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    auth: Option<RpcAuth>,
}

fn rpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

async fn dispatch_request(request: RpcRequest, rpc: &CoworkRpc) -> Option<Value> {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return id.map(|id| rpc_error(Some(id), -32600, "Invalid Request: jsonrpc must be 2.0"));
    }
    // Capability gate (S-1): reject everything - including notifications -
    // before any method handler runs.
    if !rpc.authorize(&request) {
        return Some(rpc_error(
            id,
            -32001,
            "Unauthorized: missing or invalid Cowork capability token",
        ));
    }
    let Some(id) = id else {
        return None;
    };

    let result = match request.method.as_str() {
        "ping" => Ok(json!({ "name": "jcode-cowork", "version": 1 })),
        "mount_folder" => {
            let session_id = request.params.get("session_id").and_then(Value::as_str);
            let path = request.params.get("path").and_then(Value::as_str);
            match (session_id, path) {
                (Some(session_id), Some(path)) if !session_id.trim().is_empty() => rpc
                    .mount_folder(session_id, Path::new(path))
                    .await
                    .map(|path| json!({ "path": path }))
                    .map_err(|error| (-32000, error.to_string())),
                _ => Err((
                    -32602,
                    "params require non-empty string session_id and string path".to_string(),
                )),
            }
        }
        "unmount_folder" => {
            let session_id = request.params.get("session_id").and_then(Value::as_str);
            let path = request.params.get("path").and_then(Value::as_str);
            match (session_id, path) {
                (Some(session_id), Some(path)) if !session_id.trim().is_empty() => rpc
                    .unmount_folder(session_id, Path::new(path))
                    .await
                    .map(|removed| json!({ "removed": removed }))
                    .map_err(|error| (-32000, error.to_string())),
                _ => Err((
                    -32602,
                    "params require non-empty string session_id and string path".to_string(),
                )),
            }
        }
        "start_task" => {
            let goal = request.params.get("goal").and_then(Value::as_str);
            let allowed_roots = request
                .params
                .get("allowed_roots")
                .and_then(Value::as_array)
                .and_then(|roots| {
                    roots
                        .iter()
                        .map(|root| root.as_str().map(PathBuf::from))
                        .collect::<Option<Vec<_>>>()
                });
            match (goal, allowed_roots) {
                (Some(goal), Some(roots)) => rpc
                    .start_task(goal.to_owned(), roots)
                    .await
                    .map(|task_id| json!({ "task_id": task_id }))
                    .map_err(|error| (-32000, error.to_string())),
                _ => Err((
                    -32602,
                    "params require string goal and string-array allowed_roots".to_string(),
                )),
            }
        }
        "stop_task" => {
            let task_id = request.params.get("task_id").and_then(Value::as_str);
            match task_id {
                Some(task_id) => Ok(json!({ "stopped": rpc.stop_task(task_id).await })),
                None => Err((-32602, "params require string task_id".to_string())),
            }
        }
        "approve_tool_call" => {
            let approval_id = request.params.get("id").and_then(Value::as_str);
            let approved = request.params.get("approved").and_then(Value::as_bool);
            match (approval_id, approved) {
                (Some(approval_id), Some(approved)) => {
                    Ok(json!({ "resolved": rpc.approve_tool_call(approval_id, approved).await }))
                }
                _ => Err((
                    -32602,
                    "params require string id and boolean approved".to_string(),
                )),
            }
        }
        _ => Err((-32601, format!("Method not found: {}", request.method))),
    };

    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(Some(id), code, message),
    })
}

/// Read one newline-terminated line, rejecting input beyond
/// [`MAX_REQUEST_BYTES`] (S-7). Unlike `AsyncBufReadExt::next_line`, the
/// buffer is filled incrementally so an oversized line fails fast instead of
/// being fully buffered first.
async fn read_bounded_line<R>(reader: &mut R) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&buf).into_owned())
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map(|pos| pos + 1).unwrap_or(available.len());
        if buf.len() + take > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request exceeds the Cowork IPC size limit",
            ));
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            // Match AsyncBufReadExt::next_line semantics: strip the trailing
            // newline (and a preceding carriage return) from the returned line.
            if buf.last() == Some(&b'\n') {
                buf.pop();
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
            }
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

#[cfg(windows)]
pub async fn run_server() -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ServerOptions;

    let rpc = global().clone();
    let token_path = rpc.publish_capability_token().await?;
    tracing::info!(
        "Cowork IPC capability token published at {}",
        token_path.display()
    );
    let mut first_instance = true;
    loop {
        let server = ServerOptions::new()
            .first_pipe_instance(first_instance)
            // S-10 hardening: refuse pipe connections over the network.
            .reject_remote_clients(true)
            .create(PIPE_NAME)
            .with_context(|| format!("failed to create named pipe {PIPE_NAME}"))?;
        first_instance = false;
        server
            .connect()
            .await
            .context("failed accepting Cowork IPC client")?;

        let client_rpc = rpc.clone();
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader);
            let mut events = client_rpc.subscribe();
            // Events flow only after the connection proves it holds the
            // capability token, so approval IDs are never broadcast to
            // unauthenticated listeners (S-1).
            let mut authorized = false;
            loop {
                tokio::select! {
                    line = read_bounded_line(&mut lines) => {
                        let Ok(Some(line)) = line else { break };
                        let response = match serde_json::from_str::<RpcRequest>(&line) {
                            Ok(request) => {
                                if client_rpc.authorize(&request) {
                                    authorized = true;
                                }
                                dispatch_request(request, &client_rpc).await
                            }
                            Err(error) => Some(rpc_error(None, -32700, format!("Parse error: {error}"))),
                        };
                        if let Some(response) = response {
                            if writer.write_all(&serde_json::to_vec(&response).unwrap_or_default()).await.is_err()
                                || writer.write_all(b"\n").await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    event = events.recv() => {
                        let Ok(event) = event else { continue };
                        if !authorized {
                            continue;
                        }
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": "cowork/event",
                            "params": event,
                        });
                        if writer.write_all(&serde_json::to_vec(&notification).unwrap_or_default()).await.is_err()
                            || writer.write_all(b"\n").await.is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
}

#[cfg(not(windows))]
pub async fn run_server() -> Result<()> {
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async_with_config;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

    const WS_PORT: u16 = 29384;
    let addr = format!("127.0.0.1:{}", WS_PORT);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind WebSocket server to {}", addr))?;

    let rpc = global().clone();
    let token_path = rpc.publish_capability_token().await?;
    tracing::info!(
        "Cowork WebSocket IPC server listening on ws://{}; capability token published at {}",
        addr,
        token_path.display()
    );

    // S-7: cap message and frame sizes so a client cannot exhaust daemon
    // memory with one oversized frame. The bind is loopback-only; the
    // capability token below is what actually authorizes each request (S-2).
    let ws_config = WebSocketConfig {
        max_message_size: Some(MAX_REQUEST_BYTES),
        max_frame_size: Some(MAX_REQUEST_BYTES),
        ..Default::default()
    };

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept TCP connection")?;
        let client_rpc = rpc.clone();

        tokio::spawn(async move {
            let ws_stream = match accept_async_with_config(stream, Some(ws_config)).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::warn!("WebSocket handshake failed: {}", e);
                    return;
                }
            };

            let (mut ws_sink, mut ws_stream) = ws_stream.split();
            let mut events = client_rpc.subscribe();
            // Events flow only after the connection proves it holds the
            // capability token (S-1).
            let mut authorized = false;

            loop {
                tokio::select! {
                    msg = ws_stream.next() => {
                        let Some(Ok(msg)) = msg else { break };
                        if msg.is_text() {
                            let text = msg.into_text().unwrap_or_default();
                            let response = match serde_json::from_str::<RpcRequest>(&text) {
                                Ok(request) => {
                                    if client_rpc.authorize(&request) {
                                        authorized = true;
                                    }
                                    dispatch_request(request, &client_rpc).await
                                }
                                Err(error) => Some(rpc_error(None, -32700, format!("Parse error: {error}"))),
                            };
                            if let Some(response) = response {
                                let json = serde_json::to_string(&response).unwrap_or_default();
                                if ws_sink.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                            }
                        } else if msg.is_close() {
                            break;
                        }
                    }
                    event = events.recv() => {
                        let Ok(event) = event else { continue };
                        if !authorized {
                            continue;
                        }
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": "cowork/event",
                            "params": event,
                        });
                        let json = serde_json::to_string(&notification).unwrap_or_default();
                        if ws_sink.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    /// Build a request that carries the daemon's capability token.
    fn authed_request(rpc: &CoworkRpc, id: i64, method: &str, params: Value) -> RpcRequest {
        RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(id)),
            method: method.to_string(),
            params,
            auth: Some(RpcAuth {
                token: rpc.inner.auth_token.clone(),
            }),
        }
    }

    #[tokio::test]
    async fn approval_waits_for_explicit_decision() {
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        let request_rpc = rpc.clone();
        let request = tokio::spawn(async move {
            request_rpc
                .request_tool_approval("call-1", "delete_file", "Remove report.docx")
                .await
        });

        let event = events.recv().await.unwrap();
        assert_eq!(
            event,
            CoworkEvent::ApprovalRequired {
                id: "call-1".to_string(),
                tool_name: "delete_file".to_string(),
                description: "Remove report.docx".to_string(),
            }
        );
        assert!(!request.is_finished());
        assert!(rpc.approve_tool_call("call-1", false).await);
        assert!(!request.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn unknown_approval_does_not_resolve() {
        let rpc = CoworkRpc::new();
        assert!(!rpc.approve_tool_call("missing", true).await);
    }

    #[tokio::test]
    async fn mount_folder_returns_canonical_root_and_registers_it() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = CoworkRpc::new();
        let expected = tokio::fs::canonicalize(directory.path()).await.unwrap();
        let response = dispatch_request(
            authed_request(
                &rpc,
                1,
                "mount_folder",
                json!({
                    "session_id": "gui-session",
                    "path": directory.path(),
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();

        assert_eq!(response["result"]["path"], json!(expected));
        assert_eq!(
            rpc.inner.mounted_roots.lock().await["gui-session"],
            vec![expected]
        );
    }

    #[tokio::test]
    async fn start_task_rejects_malformed_root_arrays_without_dropping_entries() {
        let rpc = CoworkRpc::new();
        let response = dispatch_request(
            authed_request(
                &rpc,
                2,
                "start_task",
                json!({
                    "goal": "Review these files",
                    "allowed_roots": ["/tmp", 7],
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();

        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn start_task_returns_id_and_stop_emits_one_completion_event() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        let task_id = rpc
            .start_task_with(
                "Review these files".to_string(),
                vec![directory.path().to_path_buf()],
                |_, _, _, _| async { std::future::pending::<Result<String>>().await },
            )
            .await
            .unwrap();
        assert!(!task_id.is_empty());

        let response = dispatch_request(
            authed_request(&rpc, 3, "stop_task", json!({ "task_id": task_id })),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["stopped"], true);
        assert_eq!(
            events.recv().await.unwrap(),
            CoworkEvent::TaskCompleted {
                task_id,
                success: false,
                summary: Some("Task stopped by user".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn completed_task_emits_its_result() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        let task_id = rpc
            .start_task_with(
                "Review these files".to_string(),
                vec![directory.path().to_path_buf()],
                |_, _, _, _| async { Ok("Reviewed the workspace".to_string()) },
            )
            .await
            .unwrap();

        assert_eq!(
            events.recv().await.unwrap(),
            CoworkEvent::TaskCompleted {
                task_id,
                success: true,
                summary: Some("Reviewed the workspace".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn ping_uses_json_rpc_2_response_shape() {
        let rpc = CoworkRpc::new();
        let response = dispatch_request(
            authed_request(&rpc, 7, "ping", Value::Null),
            &rpc,
        )
        .await
        .unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"]["name"], "jcode-cowork");
    }

    #[tokio::test]
    async fn unauthorized_requests_are_rejected_before_any_handler_runs() {
        let rpc = CoworkRpc::new();
        // Missing token entirely.
        let response = dispatch_request(
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(8)),
                method: "mount_folder".to_string(),
                params: json!({ "session_id": "s", "path": "/" }),
                auth: None,
            },
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32001);

        // Wrong token.
        let response = dispatch_request(
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(9)),
                method: "start_task".to_string(),
                params: json!({ "goal": "g", "allowed_roots": [] }),
                auth: Some(RpcAuth {
                    token: "not-the-real-token".to_string(),
                }),
            },
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32001);

        // And the rejected start_task must not have started anything.
        assert!(rpc.inner.tasks.lock().await.is_empty());
    }

    #[tokio::test]
    async fn bounded_line_reader_rejects_oversized_lines() {
        let mut oversized = vec![b'a'; MAX_REQUEST_BYTES + 10];
        oversized.push(b'\n');
        let mut reader = BufReader::new(std::io::Cursor::new(oversized));
        let error = read_bounded_line(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn bounded_line_reader_handles_lines_and_partial_eof() {
        let data = b"{\"ok\":1}\npartial-without-newline";
        let mut reader = BufReader::new(std::io::Cursor::new(data.to_vec()));
        assert_eq!(
            read_bounded_line(&mut reader).await.unwrap().unwrap(),
            "{\"ok\":1}"
        );
        assert_eq!(
            read_bounded_line(&mut reader).await.unwrap().unwrap(),
            "partial-without-newline"
        );
        assert!(read_bounded_line(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn capability_tokens_are_unique_per_daemon_instance() {
        let a = CoworkRpc::new();
        let b = CoworkRpc::new();
        assert_ne!(a.inner.auth_token, b.inner.auth_token);
        assert_eq!(a.inner.auth_token.len(), 64);
        assert!(
            a.inner
                .auth_token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
    }
}

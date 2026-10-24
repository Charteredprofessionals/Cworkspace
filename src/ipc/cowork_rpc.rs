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
                events,
                approvals: Mutex::new(HashMap::new()),
                mounted_roots: Mutex::new(HashMap::new()),
                jail_registrations: Mutex::new(HashMap::new()),
                approval_registrations: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
            }),
        }
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
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
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

#[cfg(windows)]
pub async fn run_server() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    let rpc = global().clone();
    let mut first_instance = true;
    loop {
        let server = ServerOptions::new()
            .first_pipe_instance(first_instance)
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
            let mut lines = BufReader::new(reader).lines();
            let mut events = client_rpc.subscribe();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let response = match serde_json::from_str::<RpcRequest>(&line) {
                            Ok(request) => dispatch_request(request, &client_rpc).await,
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
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    const WS_PORT: u16 = 29384;
    let addr = format!("127.0.0.1:{}", WS_PORT);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind WebSocket server to {}", addr))?;

    let rpc = global().clone();
    tracing::info!("Cowork WebSocket IPC server listening on ws://{}", addr);

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept TCP connection")?;
        let client_rpc = rpc.clone();

        tokio::spawn(async move {
            let ws_stream = match accept_async(stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::warn!("WebSocket handshake failed: {}", e);
                    return;
                }
            };

            let (mut ws_sink, mut ws_stream) = ws_stream.split();
            let mut events = client_rpc.subscribe();

            loop {
                tokio::select! {
                    msg = ws_stream.next() => {
                        let Some(Ok(msg)) = msg else { break };
                        if msg.is_text() {
                            let text = msg.into_text().unwrap_or_default();
                            let response = match serde_json::from_str::<RpcRequest>(&text) {
                                Ok(request) => dispatch_request(request, &client_rpc).await,
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
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "mount_folder".to_string(),
                params: json!({
                    "session_id": "gui-session",
                    "path": directory.path(),
                }),
            },
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
        let response = dispatch_request(
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(2)),
                method: "start_task".to_string(),
                params: json!({
                    "goal": "Review these files",
                    "allowed_roots": ["/tmp", 7],
                }),
            },
            &CoworkRpc::new(),
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
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(3)),
                method: "stop_task".to_string(),
                params: json!({ "task_id": task_id }),
            },
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
        let response = dispatch_request(
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(7)),
                method: "ping".to_string(),
                params: Value::Null,
            },
            &CoworkRpc::new(),
        )
        .await
        .unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"]["name"], "jcode-cowork");
    }
}

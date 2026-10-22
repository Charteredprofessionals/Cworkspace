use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use uuid::Uuid;

const PIPE_NAME: &str = r"\\.\pipe\jcode-cowork";
const KNOWN_EVENTS: [&str; 5] = [
    "AgentSpawned",
    "ToolInvoked",
    "ArtifactCreated",
    "ApprovalRequired",
    "TaskCompleted",
];

struct Bridge {
    connection: Mutex<Option<Arc<RpcConnection>>>,
    child: Mutex<Option<Child>>,
    mounted_roots: Mutex<Vec<PathBuf>>,
    session_id: String,
}

impl Bridge {
    fn session_id(&self) -> String {
        self.session_id.clone()
    }

    fn shutdown(&self) {
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        if let Ok(mut connection) = self.connection.lock() {
            if let Some(connection) = connection.take() {
                connection.close();
            }
        }
    }
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            connection: Mutex::new(None),
            child: Mutex::new(None),
            mounted_roots: Mutex::new(Vec::new()),
            session_id: Uuid::new_v4().to_string(),
        }
    }
}

struct RpcConnection {
    writer: Mutex<Option<File>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
}

impl RpcConnection {
    fn close(&self) {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }
        if let Ok(mut waiting) = self.pending.lock() {
            for (_, sender) in waiting.drain() {
                let _ = sender.send(Err("The Cowork desktop app is closing".to_owned()));
            }
        }
    }

    fn open(app: AppHandle) -> Result<Arc<Self>, String> {
        let pipe = open_named_pipe()?;
        let reader = pipe.try_clone().map_err(|error| error.to_string())?;
        let connection = Arc::new(Self {
            writer: Mutex::new(Some(pipe)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
        });
        let pending = Arc::clone(&connection.pending);
        let connection_alive = Arc::clone(&connection);

        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let Ok(message) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if let Some(id) = message.get("id").and_then(Value::as_u64) {
                            if let Ok(mut waiting) = pending.lock() {
                                if let Some(sender) = waiting.remove(&id) {
                                    let response = if let Some(error) = message.get("error") {
                                        Err(error
                                            .get("message")
                                            .and_then(Value::as_str)
                                            .unwrap_or("Daemon request failed")
                                            .to_owned())
                                    } else {
                                        Ok(message.get("result").cloned().unwrap_or(Value::Null))
                                    };
                                    let _ = sender.send(response);
                                }
                            }
                        } else if let Some(event) = daemon_event(&message) {
                            let _ = app.emit("cowork-event", event);
                        }
                    }
                }
            }
            connection_alive.alive.store(false, Ordering::Release);
            if let Ok(mut waiting) = pending.lock() {
                for (_, sender) in waiting.drain() {
                    let _ = sender.send(Err("The jcode Cowork daemon disconnected".to_owned()));
                }
            }
        });
        Ok(connection)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| "RPC response map is unavailable".to_owned())?
            .insert(id, sender);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let write_result = (|| {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| "RPC writer is unavailable".to_owned())?;
            let writer = writer
                .as_mut()
                .ok_or_else(|| "RPC pipe is closed".to_owned())?;
            serde_json::to_writer(&mut *writer, &request).map_err(|error| error.to_string())?;
            writer.write_all(b"\n").map_err(|error| error.to_string())?;
            writer.flush().map_err(|error| error.to_string())
        })();
        if let Err(error) = write_result {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }

        receiver
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| format!("Timed out waiting for daemon method {method}"))?
    }
}

fn daemon_event(message: &Value) -> Option<Value> {
    let params = message.get("params").unwrap_or(message);
    let name = message
        .get("event")
        .and_then(Value::as_str)
        .or_else(|| params.get("event").and_then(Value::as_str))
        .or_else(|| params.get("type").and_then(Value::as_str))
        .or_else(|| message.get("method").and_then(Value::as_str))?;
    if !KNOWN_EVENTS.contains(&name) {
        return None;
    }
    let data = params.get("data").cloned().unwrap_or_else(|| params.clone());
    Some(json!({ "event": name, "data": data }))
}

fn open_named_pipe() -> Result<File, String> {
    #[cfg(windows)]
    {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE_NAME)
            .map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = PIPE_NAME;
        Err("The jcode Cowork named-pipe bridge is available only on Windows".to_owned())
    }
}

fn launch_daemon(bridge: &Bridge) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut child_slot = bridge
            .child
            .lock()
            .map_err(|_| "Daemon process state is unavailable".to_owned())?;
        if let Some(child) = child_slot.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                return Ok(());
            }
            *child_slot = None;
        }

        let binary = std::env::var_os("JCODE_BIN")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().map(|parent| parent.join("jcode.exe")))
                    .filter(|path| path.exists())
            })
            .unwrap_or_else(|| PathBuf::from("jcode.exe"));
        let mut command = Command::new(binary);
        command
            .arg("--cowork-daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
        *child_slot = Some(command.spawn().map_err(|error| {
            format!("Could not start jcode --cowork-daemon: {error}")
        })?);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = bridge;
        Err("Starting the Cowork daemon is supported only on Windows".to_owned())
    }
}

fn ensure_connection(app: &AppHandle, bridge: &Bridge) -> Result<Arc<RpcConnection>, String> {
    let mut slot = bridge
        .connection
        .lock()
        .map_err(|_| "Daemon connection state is unavailable".to_owned())?;
    if let Some(connection) = slot.as_ref() {
        if connection.alive.load(Ordering::Acquire) {
            return Ok(Arc::clone(connection));
        }
    }

    match RpcConnection::open(app.clone()) {
        Ok(connection) => {
            connection.request("ping", json!({}))?;
            *slot = Some(Arc::clone(&connection));
            return Ok(connection);
        }
        Err(_) => launch_daemon(bridge)?,
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match RpcConnection::open(app.clone()) {
            Ok(connection) => match connection.request("ping", json!({})) {
                Ok(_) => {
                    *slot = Some(Arc::clone(&connection));
                    return Ok(connection);
                }
                Err(error) => last_error = error,
            },
            Err(error) => last_error = error,
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(if last_error.is_empty() {
        "Could not connect to the jcode Cowork daemon".to_owned()
    } else {
        last_error
    })
}

#[tauri::command]
async fn connect_daemon(app: AppHandle, bridge: State<'_, Bridge>) -> Result<(), String> {
    ensure_connection(&app, &bridge).map(|_| ())
}

fn canonical_folder(path: &str) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    if !canonical.is_dir() {
        return Err("Choose a folder to mount".to_owned());
    }
    Ok(canonical)
}

#[tauri::command]
async fn mount_folder(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    path: String,
) -> Result<String, String> {
    let canonical = canonical_folder(&path)?;
    let connection = ensure_connection(&app, &bridge)?;
    connection.request(
        "mount_folder",
        json!({
            "session_id": bridge.session_id(),
            "path": canonical.to_string_lossy(),
        }),
    )?;
    let mut roots = bridge
        .mounted_roots
        .lock()
        .map_err(|_| "Mounted folder state is unavailable".to_owned())?;
    if !roots.contains(&canonical) {
        roots.push(canonical.clone());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

#[tauri::command]
async fn unmount_folder(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    path: String,
) -> Result<(), String> {
    let canonical = canonical_folder(&path)?;
    let connection = ensure_connection(&app, &bridge)?;
    connection.request(
        "unmount_folder",
        json!({
            "session_id": bridge.session_id(),
            "path": canonical.to_string_lossy(),
        }),
    )?;
    if let Ok(mut roots) = bridge.mounted_roots.lock() {
        roots.retain(|root| root != &canonical);
    }
    Ok(())
}

#[tauri::command]
async fn start_cowork_task(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    goal: String,
    allowed_roots: Vec<String>,
) -> Result<String, String> {
    let goal = goal.trim();
    if goal.is_empty() {
        return Err("Enter a goal before starting a task".to_owned());
    }
    let mounted = bridge
        .mounted_roots
        .lock()
        .map_err(|_| "Mounted folder state is unavailable".to_owned())?
        .clone();
    let allowed = allowed_roots
        .iter()
        .map(|path| canonical_folder(path))
        .collect::<Result<Vec<_>, _>>()?;
    if allowed.is_empty() || allowed.iter().any(|path| !mounted.contains(path)) {
        return Err("Tasks can use only folders mounted in this session".to_owned());
    }
    let connection = ensure_connection(&app, &bridge)?;
    let response = connection.request(
        "start_task",
        json!({
            "goal": goal,
            "allowed_roots": allowed.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
        }),
    )?;
    response
        .as_str()
        .or_else(|| response.get("task_id").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| "Daemon response did not include task_id".to_owned())
}

#[tauri::command]
async fn stop_task(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    task_id: String,
) -> Result<(), String> {
    let connection = ensure_connection(&app, &bridge)?;
    connection.request("stop_task", json!({ "task_id": task_id }))?;
    Ok(())
}

#[tauri::command]
async fn respond_approval(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    id: Value,
    approved: bool,
) -> Result<(), String> {
    let connection = ensure_connection(&app, &bridge)?;
    connection.request("approve_tool_call", json!({ "id": id, "approved": approved }))?;
    Ok(())
}

#[derive(Serialize)]
struct ArtifactPayload {
    name: String,
    mime_type: String,
    content_base64: String,
}

#[tauri::command]
fn read_artifact(bridge: State<'_, Bridge>, path: String) -> Result<ArtifactPayload, String> {
    let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
    let mounted = bridge
        .mounted_roots
        .lock()
        .map_err(|_| "Mounted folder state is unavailable".to_owned())?;
    if !mounted.iter().any(|root| canonical.starts_with(root)) {
        return Err("Artifact is outside the mounted workspace folders".to_owned());
    }
    let metadata = fs::metadata(&canonical).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > 30 * 1024 * 1024 {
        return Err("Artifact must be a file smaller than 30 MB".to_owned());
    }
    let extension = canonical
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime_type = match extension.as_str() {
        "md" | "markdown" => "text/markdown",
        "pdf" => "application/pdf",
        "xlsx" | "xls" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => return Err("Preview supports Markdown, PDF, XLSX, and DOCX files".to_owned()),
    };
    let bytes = fs::read(&canonical).map_err(|error| error.to_string())?;
    Ok(ArtifactPayload {
        name: canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Artifact")
            .to_owned(),
        mime_type: mime_type.to_owned(),
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

pub fn run() {
    let bridge = Bridge {
        session_id: Uuid::new_v4().to_string(),
        ..Bridge::default()
    };
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(bridge)
        .invoke_handler(tauri::generate_handler![
            connect_daemon,
            mount_folder,
            unmount_folder,
            start_cowork_task,
            stop_task,
            respond_approval,
            read_artifact
        ])
        .build(tauri::generate_context!())
        .expect("error while building jcode Cowork")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit) {
                if let Some(bridge) = app.try_state::<Bridge>() {
                    bridge.shutdown();
                }
            }
        });
}
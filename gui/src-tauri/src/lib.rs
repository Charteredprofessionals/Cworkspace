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
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent, Wry};
use uuid::Uuid;

const PIPE_NAME: &str = r"\\.\pipe\jcode-cowork";
/// Must stay in sync with `src/ipc/cowork_rpc.rs` (`token_file_path`): the
/// daemon publishes a 256-bit capability token here before accepting pipe
/// clients, and every RPC request must carry it or the daemon rejects the
/// call as unauthorized.
const TOKEN_FILE_NAME: &str = "jcode-cowork-ipc.token";
const KNOWN_EVENTS: [&str; 5] = [
    "AgentSpawned",
    "ToolInvoked",
    "ArtifactCreated",
    "ApprovalRequired",
    "TaskCompleted",
];

fn read_capability_token() -> Result<String, String> {
    let path = std::env::temp_dir().join(TOKEN_FILE_NAME);
    fs::read_to_string(&path)
        .map(|token| token.trim().to_owned())
        .map_err(|error| {
            format!(
                "Cowork capability token not found at {}: {error}. Is the daemon running?",
                path.display()
            )
        })
        .and_then(|token| {
            if token.is_empty() {
                Err("Cowork capability token file is empty".to_owned())
            } else {
                Ok(token)
            }
        })
}

fn daemon_log_path() -> PathBuf {
    std::env::temp_dir().join("jcode-cowork-daemon.log")
}

/// Rebuild the union of project mount roots from the daemon's project list.
/// Best-effort: on failure the previous list stays (previews degrade, they
/// never break).
fn refresh_project_roots(connection: &RpcConnection, bridge: &Bridge) {
    // `request` returns Result<Value, String>; a failed call keeps the
    // previous root list (previews degrade, they never break).
    let Ok(list) = connection.request("project.list", json!({})) else {
        return;
    };
    let Some(projects) = list.get("projects").and_then(Value::as_array) else {
        return;
    };
    let mut roots = Vec::new();
    for project in projects {
        if let Some(mounts) = project.get("mounts").and_then(Value::as_array) {
            for mount in mounts {
                if let Some(path) = mount.get("path").and_then(Value::as_str) {
                    let path = PathBuf::from(path);
                    if !roots.contains(&path) {
                        roots.push(path);
                    }
                }
            }
        }
    }
    if let Ok(mut slot) = bridge.project_roots.lock() {
        *slot = roots;
    }
}

struct Bridge {
    connection: Mutex<Option<Arc<RpcConnection>>>,
    child: Mutex<Option<Child>>,
    /// Windows job object wrapping the daemon child — a tracking handle the
    /// quit path uses to terminate the daemon group explicitly. Since v0.1.7
    /// it deliberately has NO kill-on-close flag: the daemon (and its running
    /// tasks) survives plain GUI exits and even a GUI crash, adopting the
    /// "background execution" model; an idle janitor exits an orphaned
    /// daemon after ten quiet minutes, and the NSIS installer hooks taskkill
    /// jcode.exe so upgrades and uninstalls are never blocked. Raw handle
    /// stored as `isize` to keep `Bridge` Send + Sync.
    job: Mutex<Option<isize>>,
    mounted_roots: Mutex<Vec<PathBuf>>,
    /// Union of every project's mount roots (daemon-side). Project-scoped
    /// tasks never session-mount, so artifact previews must also accept
    /// files living inside project mounts.
    project_roots: Mutex<Vec<PathBuf>>,
    session_id: String,
    /// Set only by the tray's Quit item: an explicit quit stops the daemon
    /// (graceful RPC, then a job/child kill as fallback). Any other exit —
    /// crash, OS logoff, dev restart — leaves the daemon running.
    quitting: AtomicBool,
    /// True when this GUI session attached to an ALREADY RUNNING daemon
    /// instead of spawning one; the frontend surfaces a reattach notice.
    adopted: AtomicBool,
}

impl Bridge {
    fn session_id(&self) -> String {
        self.session_id.clone()
    }

    fn shutdown(&self) {
        // Explicit-quit cleanup only (the run closure gates this on
        // `quitting`): terminate the daemon group through the job handle,
        // then release it. The graceful `shutdown` RPC usually wins the
        // race; this is the fallback that also covers a hung daemon.
        #[cfg(windows)]
        if let Ok(job) = self.job.lock() {
            if let Some(handle) = job.as_ref() {
                unsafe {
                    windows_sys::Win32::System::JobObjects::TerminateJobObject(*handle, 0);
                    windows_sys::Win32::Foundation::CloseHandle(*handle);
                }
            }
        }
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
            job: Mutex::new(None),
            mounted_roots: Mutex::new(Vec::new()),
            project_roots: Mutex::new(Vec::new()),
            session_id: Uuid::new_v4().to_string(),
            quitting: AtomicBool::new(false),
            adopted: AtomicBool::new(false),
        }
    }
}

struct RpcConnection {
    writer: Mutex<Option<File>>,
    auth_token: String,
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
                let _ = sender.send(Err("The Cworkspace desktop app is closing".to_owned()));
            }
        }
    }

    fn open(app: AppHandle) -> Result<Arc<Self>, String> {
        let auth_token = read_capability_token()?;
        let pipe = open_named_pipe()?;
        let reader = pipe.try_clone().map_err(|error| error.to_string())?;
        let connection = Arc::new(Self {
            writer: Mutex::new(Some(pipe)),
            auth_token,
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
                    let _ = sender.send(Err("The Cworkspace daemon disconnected".to_owned()));
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
            "auth": { "token": self.auth_token },
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
        Err("The Cworkspace named-pipe bridge is available only on Windows".to_owned())
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
        // The daemon reports startup failures (anyhow errors, panics) on
        // stderr, which is invisible to the desktop app. Capture them in a
        // log file so a daemon that dies before publishing its capability
        // token is diagnosable instead of silent.
        let daemon_log = daemon_log_path();
        let stderr = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&daemon_log)
            .map(Stdio::from)
            .unwrap_or(Stdio::null());
        let mut command = Command::new(binary);
        command
            .arg("--cowork-daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
        *child_slot = Some(command.spawn().map_err(|error| {
            format!("Could not start jcode --cowork-daemon: {error}")
        })?);
        // Tie the daemon to the GUI's lifetime with a kill-on-close job
        // object: whatever ends the GUI (clean exit, crash, a forced
        // termination from an uninstaller or the task manager) also ends
        // the daemon, so it can never outlive the app and keep the install
        // directory's files in use.
        if let Some(child) = child_slot.as_mut() {
            unsafe { attach_daemon_job(&bridge.job, child) };
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = bridge;
        Err("Starting the Cworkspace daemon is supported only on Windows".to_owned())
    }
}

/// Put `child` into a fresh job object and remember the job handle in
/// `slot` so `Bridge::shutdown` can terminate the daemon group explicitly
/// on an explicit quit. Deliberately WITHOUT the kill-on-close flag since
/// v0.1.7: the daemon (and its running tasks) must survive plain GUI exits
/// and GUI crashes — background execution. The idle janitor inside the
/// daemon cleans up an orphaned daemon after ten quiet minutes, and the
/// NSIS installer hooks taskkill jcode.exe for upgrades/uninstalls.
/// Best-effort: if job creation or assignment fails the daemon still runs,
/// just untracked (e.g. hosts too old for nested jobs).
#[cfg(windows)]
unsafe fn attach_daemon_job(slot: &Mutex<Option<isize>>, child: &mut Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};

    let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
    if job == 0 {
        return;
    }
    // HANDLE is an isize in windows-sys 0.52; the child's raw handle is a
    // pointer, so cast through the pointer's numeric value.
    if AssignProcessToJobObject(job, child.as_raw_handle() as isize) == 0 {
        CloseHandle(job);
        return;
    }
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(job);
    } else {
        CloseHandle(job);
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
            // Attached to an already-running daemon (a previous GUI session
            // or one left running with background tasks) instead of spawning
            // one — the frontend surfaces a reattach notice.
            bridge.adopted.store(true, Ordering::SeqCst);
            *slot = Some(Arc::clone(&connection));
            return Ok(connection);
        }
        Err(_) => launch_daemon(bridge)?,
    }

    // First launch can exceed the daemon's millisecond-scale token publish
    // by many seconds on Windows: Defender scans a freshly installed
    // executable before its first run. Retry well past that.
    let deadline = Instant::now() + Duration::from_secs(20);
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
        // If the spawned daemon already terminated, retrying the pipe is
        // pointless — report why it died instead of a timeout-shaped error.
        // try_lock avoids inverting the child/connection lock order used by
        // Bridge::shutdown; skipping the check on contention is harmless.
        if let Ok(mut child_slot) = bridge.child.try_lock() {
            if let Some(child) = child_slot.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    let exit = status
                        .code()
                        .map(|code| format!("exit code {code}"))
                        .unwrap_or_else(|| "non-zero exit".to_owned());
                    return Err(format!(
                        "jcode --cowork-daemon exited with {exit} before accepting connections; \
                         daemon log: {} (last error: {last_error})",
                        daemon_log_path().display()
                    ));
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(if last_error.is_empty() {
        "Could not connect to the Cworkspace daemon".to_owned()
    } else {
        last_error
    })
}

#[tauri::command]
async fn connect_daemon(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    // Opening the pipe, the ping handshake, and the launch retry loop are
    // blocking; keep them off the shared runtime workers so a slow daemon
    // start never stalls other IPC commands.
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        let adopted = bridge.adopted.load(Ordering::SeqCst);
        let running_tasks = connection
            .request("task.list", json!({}))
            .ok()
            .and_then(|value| {
                value
                    .get("tasks")
                    .and_then(Value::as_array)
                    .map(|tasks| tasks.len())
            })
            .unwrap_or(0);
        Ok(json!({ "adopted": adopted, "running_tasks": running_tasks }))
    })
}

/// Show (and focus) the main window from the tray.
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Explicit quit from the tray: mark the session as quitting so the Exit
/// path is allowed to kill the daemon, ask it to stop gracefully first
/// (aborting running tasks), then leave. The graceful request runs on a
/// thread without waiting — a hung daemon must not block quitting, and
/// `Bridge::shutdown` at Exit is the fallback kill.
fn quit_application(app: &AppHandle) {
    if let Some(bridge) = app.try_state::<Bridge>() {
        bridge.quitting.store(true, Ordering::SeqCst);
        let connection = bridge
            .connection
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone));
        thread::spawn(move || {
            if let Some(connection) = connection {
                if connection.alive.load(Ordering::Acquire) {
                    let _ = connection.request("shutdown", json!({}));
                }
            }
        });
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    app.exit(0);
}

/// Keep the tray's task counter current by polling the daemon's task list
/// every 20 seconds. Polling (instead of counting events) stays correct
/// across reattach: the count reflects what the daemon itself believes.
fn tray_task_watcher(app: AppHandle, item: MenuItem<Wry>) {
    loop {
        thread::sleep(Duration::from_secs(20));
        let Some(bridge) = app.try_state::<Bridge>() else { return };
        let connection = bridge
            .connection
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone));
        let label = match connection {
            Some(connection) if connection.alive.load(Ordering::Acquire) => {
                match connection.request("task.list", json!({})) {
                    Ok(value) => {
                        let count = value
                            .get("tasks")
                            .and_then(Value::as_array)
                            .map(|tasks| tasks.len())
                            .unwrap_or(0);
                        match count {
                            0 => "No tasks running".to_owned(),
                            1 => "1 task running".to_owned(),
                            other => format!("{other} tasks running"),
                        }
                    }
                    Err(_) => "Daemon not responding".to_owned(),
                }
            }
            _ => "Daemon stopped".to_owned(),
        };
        let _ = app.run_on_main_thread(move || {
            let _ = item.set_text(&label);
        });
    }
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
    let mounted = tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "mount_folder",
            json!({
                "session_id": bridge.session_id(),
                "path": canonical.to_string_lossy(),
            }),
        )
    });
    mounted?;
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
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "unmount_folder",
            json!({
                "session_id": bridge.session_id(),
                "path": canonical.to_string_lossy(),
            }),
        )
    })?;
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
    project_id: Option<String>,
    allowed_roots: Vec<String>,
    model: Option<String>,
    instructions: Option<String>,
) -> Result<String, String> {
    let goal = goal.trim();
    if goal.is_empty() {
        return Err("Enter a goal before starting a task".to_owned());
    }
    let model = model
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty());
    let instructions = instructions
        .map(|instructions| instructions.trim().to_owned())
        .filter(|instructions| !instructions.is_empty());
    let project_id = project_id
        .map(|project_id| project_id.trim().to_owned())
        .filter(|project_id| !project_id.is_empty());
    // Project-scoped tasks take their mounts, model default, and instructions
    // from the daemon-side project; only ad-hoc tasks need session mounts.
    let params = if let Some(project_id) = project_id {
        json!({
            "goal": goal,
            "project_id": project_id,
            "model": model,
        })
    } else {
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
        json!({
            "goal": goal,
            "allowed_roots": allowed
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>(),
            "model": model,
            "instructions": instructions,
        })
    };
    let response = tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("start_task", params)
    })?;
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
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("stop_task", json!({ "task_id": task_id }))
    })?;
    Ok(())
}

#[tauri::command]
async fn respond_approval(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    id: Value,
    approved: bool,
    remember: Option<String>,
) -> Result<(), String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "approve_tool_call",
            json!({ "id": id, "approved": approved, "remember": remember }),
        )
    })?;
    Ok(())
}

#[tauri::command]
async fn permission_list(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("permissions.list", json!({}))
    })
}

#[tauri::command]
async fn permission_add(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    tool: String,
    decision: String,
    scope: Option<String>,
    project_id: Option<String>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        let mut params = json!({ "tool": tool, "decision": decision });
        if let Some(scope) = scope {
            params["scope"] = json!(scope);
        }
        if let Some(project_id) = project_id {
            params["project_id"] = json!(project_id);
        }
        connection.request("permissions.add", params)
    })
}

#[tauri::command]
async fn permission_remove(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    id: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("permissions.remove", json!({ "id": id }))
    })
}

#[tauri::command]
async fn list_models(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    // The daemon builds routes from auto-fetched provider catalogs; this call
    // is pipe I/O + JSON, so keep it off the shared runtime workers too.
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("list_models", json!({}))
    })
}

/// Thin, explicit pass-throughs for the project/settings RPC surface. Each
/// method is named (no generic invoke) so the GUI↔daemon contract stays
/// reviewable and the capability token still gates every call daemon-side.

#[tauri::command]
async fn project_list(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("project.list", json!({}))
    })
}

#[tauri::command]
async fn project_create(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    name: String,
    icon: Option<String>,
    purpose: Option<String>,
    instructions: Option<String>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "project.create",
            json!({
                "name": name,
                "icon": icon.unwrap_or_default(),
                "purpose": purpose.unwrap_or_default(),
                "instructions": instructions.unwrap_or_default(),
            }),
        )
    })
}

#[tauri::command]
async fn project_update(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    project_id: String,
    name: Option<String>,
    icon: Option<String>,
    purpose: Option<String>,
    instructions: Option<String>,
    model_default: Option<String>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "project.update",
            json!({
                "project_id": project_id,
                "name": name,
                "icon": icon,
                "purpose": purpose,
                "instructions": instructions,
                "model_default": model_default,
            }),
        )
    })
}

#[tauri::command]
async fn project_delete(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    project_id: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("project.delete", json!({ "project_id": project_id }))
    })
}

#[tauri::command]
async fn mount_add(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    project_id: String,
    path: String,
    access: String,
) -> Result<Value, String> {
    let canonical = canonical_folder(&path)?;
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        let response = connection.request(
            "mount.add",
            json!({
                "project_id": project_id,
                "path": canonical.to_string_lossy(),
                "access": access,
            }),
        );
        refresh_project_roots(&connection, &bridge);
        response
    })
}

#[tauri::command]
async fn mount_revoke(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    project_id: String,
    path: String,
) -> Result<Value, String> {
    let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        let response = connection.request(
            "mount.revoke",
            json!({
                "project_id": project_id,
                "path": canonical.to_string_lossy(),
            }),
        );
        refresh_project_roots(&connection, &bridge);
        response
    })
}

#[tauri::command]
async fn settings_get(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("settings.get", json!({}))
    })
}

#[tauri::command]
async fn settings_update(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    approval_mode: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "settings.update",
            json!({ "approval_mode": approval_mode }),
        )
    })
}

// ---------------------------------------------------------------------------
// Protocol v3 parity surfaces: every remaining jcode capability the desktop
// was missing, bridged as thin, named pass-throughs.
// ---------------------------------------------------------------------------

#[tauri::command]
async fn task_transcript(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    task_id: String,
    limit: Option<u32>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "task.transcript",
            json!({ "task_id": task_id, "limit": limit }),
        )
    })
}

#[tauri::command]
async fn tools_list(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("tools.list", json!({}))
    })
}

#[tauri::command]
async fn usage_report(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("usage.report", json!({}))
    })
}

#[tauri::command]
async fn auth_status(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("auth.status", json!({}))
    })
}

#[tauri::command]
async fn auth_set_api_key(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    provider: String,
    api_key: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "auth.set_api_key",
            json!({ "provider": provider, "api_key": api_key }),
        )
    })
}

#[tauri::command]
async fn auth_clear(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    provider: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("auth.clear", json!({ "provider": provider }))
    })
}

#[tauri::command]
async fn mcp_list(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("mcp.list", json!({}))
    })
}

#[tauri::command]
async fn mcp_add(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    name: String,
    command: Option<String>,
    args: Option<Vec<String>>,
    url: Option<String>,
    env: Option<std::collections::HashMap<String, String>>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        let mut spec = json!({});
        if let Some(command) = command.filter(|command| !command.trim().is_empty()) {
            spec["command"] = json!(command);
        }
        if let Some(args) = args.filter(|args| !args.is_empty()) {
            spec["args"] = json!(args);
        }
        if let Some(url) = url.filter(|url| !url.trim().is_empty()) {
            spec["url"] = json!(url);
        }
        if let Some(env) = env.filter(|env| !env.is_empty()) {
            spec["env"] = json!(env);
        }
        // `json!` has no struct-update spread, so the name is inserted into
        // the spec object directly.
        let mut params = spec;
        params["name"] = json!(name);
        connection.request("mcp.add", params)
    })
}

#[tauri::command]
async fn mcp_remove(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    name: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("mcp.remove", json!({ "name": name }))
    })
}

#[tauri::command]
async fn task_followup(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    task_id: String,
    message: String,
    model: Option<String>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "task.followup",
            json!({ "task_id": task_id, "message": message, "model": model }),
        )
    })
}

#[tauri::command]
async fn schedule_create(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    project_id: String,
    goal: String,
    run_at_unix_secs: u64,
    model: Option<String>,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request(
            "schedule.create",
            json!({
                "project_id": project_id,
                "goal": goal,
                "run_at_unix_secs": run_at_unix_secs,
                "model": model,
            }),
        )
    })
}

#[tauri::command]
async fn schedule_list(app: AppHandle, bridge: State<'_, Bridge>) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("schedule.list", json!({}))
    })
}

#[tauri::command]
async fn schedule_cancel(
    app: AppHandle,
    bridge: State<'_, Bridge>,
    id: String,
) -> Result<Value, String> {
    tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("schedule.cancel", json!({ "id": id }))
    })
}

#[derive(Serialize)]
struct DaemonInfo {
    name: String,
    version: Value,
    log_path: String,
}

/// Identity + diagnostics for the Settings panel: what the ping handshake
/// reports and where a failed daemon start leaves its stderr log.
#[tauri::command]
async fn daemon_info(app: AppHandle, bridge: State<'_, Bridge>) -> Result<DaemonInfo, String> {
    let response = tokio::task::block_in_place(|| {
        let connection = ensure_connection(&app, &bridge)?;
        connection.request("ping", json!({}))
    })?;
    Ok(DaemonInfo {
        name: response
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("jcode-cowork")
            .to_owned(),
        version: response.get("version").cloned().unwrap_or(Value::Null),
        log_path: daemon_log_path().to_string_lossy().into_owned(),
    })
}

#[derive(Serialize)]
struct ArtifactPayload {
    name: String,
    mime_type: String,
    content_base64: String,
}

#[tauri::command]
async fn read_artifact(
    bridge: State<'_, Bridge>,
    path: String,
) -> Result<ArtifactPayload, String> {
    // Reading + base64-encoding up to 30 MB used to run on the UI thread and
    // froze the window on large artifacts; run it on the blocking pool with
    // owned data instead.
    let mounted_roots = bridge
        .mounted_roots
        .lock()
        .map_err(|_| "Mounted folder state is unavailable".to_owned())?
        .clone();
    let project_roots = bridge
        .project_roots
        .lock()
        .map_err(|_| "Project mount state is unavailable".to_owned())?
        .clone();
    tauri::async_runtime::spawn_blocking(move || {
        read_artifact_blocking(mounted_roots, project_roots, path)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn read_artifact_blocking(
    mounted_roots: Vec<PathBuf>,
    project_roots: Vec<PathBuf>,
    path: String,
) -> Result<ArtifactPayload, String> {
    let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
    let inside_mounted = mounted_roots
        .iter()
        .any(|root| canonical.starts_with(root));
    let inside_project = project_roots
        .iter()
        .any(|root| canonical.starts_with(root));
    if !inside_mounted && !inside_project {
        return Err(
            "Artifact is outside the mounted workspace folders and project mounts".to_owned(),
        );
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
    // Inline-preview types carry their real mime; everything else streams as
    // a download so no artifact type is a dead end in the desktop app.
    let mime_type = match extension.as_str() {
        "md" | "markdown" => "text/markdown",
        "pdf" => "application/pdf",
        "xlsx" | "xls" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" | "log" | "json" | "csv" | "toml" | "yaml" | "yml" | "rs" | "py" | "js" | "ts"
        | "tsx" | "jsx" | "html" | "css" | "xml" | "svg" => "text/plain",
        _ => "application/octet-stream",
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
        .setup(|app| {
            // Tray (v0.1.7): the GUI window can close while the daemon keeps
            // running tasks; the tray is how the user gets back in and how
            // they quit for real.
            let open_item = MenuItem::with_id(app, "open", "Open Cworkspace", true, None::<&str>)?;
            let tasks_item =
                MenuItem::with_id(app, "tasks", "No tasks running", false, None::<&str>)?;
            let quit_item = MenuItem::with_id(
                app,
                "quit",
                "Quit Cworkspace (stops background tasks)",
                true,
                None::<&str>,
            )?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &tasks_item,
                    &PredefinedMenuItem::separator(app)?,
                    &quit_item,
                ],
            )?;
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("bundle icon").clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("Cworkspace — tasks keep running in the background")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main_window(app),
                    "quit" => quit_application(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            let watcher_app = app.handle().clone();
            let watcher_item = tasks_item.clone();
            thread::spawn(move || tray_task_watcher(watcher_app, watcher_item));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect_daemon,
            list_models,
            daemon_info,
            mount_folder,
            unmount_folder,
            start_cowork_task,
            stop_task,
            respond_approval,
            read_artifact,
            project_list,
            project_create,
            project_update,
            project_delete,
            mount_add,
            mount_revoke,
            settings_get,
            settings_update,
            task_transcript,
            task_followup,
            tools_list,
            permission_list,
            permission_add,
            permission_remove,
            usage_report,
            auth_status,
            auth_set_api_key,
            auth_clear,
            mcp_list,
            mcp_add,
            mcp_remove,
            schedule_create,
            schedule_list,
            schedule_cancel
        ])
        .build(tauri::generate_context!())
        .expect("error while building Cworkspace")
        .run(|app, event| {
            match event {
                // Close-to-tray: the window hides and the daemon (with any
                // running tasks) keeps working. Quitting happens via the
                // tray menu, not the window's X button.
                RunEvent::WindowEvent {
                    label,
                    event: WindowEvent::CloseRequested { api, .. },
                    ..
                } => {
                    if label == "main" {
                        api.prevent_close();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.hide();
                        }
                    }
                }
                RunEvent::Exit => {
                    if let Some(bridge) = app.try_state::<Bridge>() {
                        // Explicit quit: stop the daemon (graceful RPC was
                        // already sent; this is the fallback kill). Any other
                        // exit — crash, OS logoff, dev restart — leaves the
                        // daemon and its running tasks alive in the
                        // background, adopted by the next GUI launch.
                        if bridge.quitting.load(Ordering::SeqCst) {
                            bridge.shutdown();
                        } else if let Ok(mut connection) = bridge.connection.lock() {
                            if let Some(connection) = connection.take() {
                                connection.close();
                            }
                        }
                    }
                }
                _ => {}
            }
        });
}
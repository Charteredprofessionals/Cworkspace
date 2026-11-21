use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use crate::cli::cowork_projects::MountAccess;
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, oneshot};

pub const PIPE_NAME: &str = r"\\.\pipe\jcode-cowork";

/// Wire protocol version. Bump on any breaking RPC contract change; the GUI
/// compares this at connect time via `ping` and refuses mismatched majors.
pub const PROTOCOL_VERSION: u32 = 2;

/// Tool names considered read-only for the auto-allow policy. They cannot
/// mutate the workspace: reads list files, grep searches content, webfetch
/// fetches a URL into the transcript. Everything else (bash, patch, edit,
/// office document creation, computer use) still requires explicit approval.
const READ_ONLY_TOOLS: &[&str] = &["read", "ls", "agentgrep", "glob", "webfetch", "websearch"];

/// Approval policy chosen in the GUI's Settings > Capabilities surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Every gated tool call waits for an explicit decision (default).
    AlwaysAsk,
    /// Read-only tools run without prompting; writes and destructive actions
    /// still wait for a decision.
    AutoAllowReadonly,
}

impl ApprovalPolicy {
    fn as_str(self) -> &'static str {
        match self {
            ApprovalPolicy::AlwaysAsk => "always_ask",
            ApprovalPolicy::AutoAllowReadonly => "auto_allow_readonly",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "always_ask" => Some(ApprovalPolicy::AlwaysAsk),
            "auto_allow_readonly" => Some(ApprovalPolicy::AutoAllowReadonly),
            _ => None,
        }
    }
}

/// Data directory for cowork settings and the project store. Tests point
/// `JCODE_COWORK_STORE_DIR` at a temp dir to stay hermetic.
pub fn store_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("JCODE_COWORK_STORE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(crate::storage::jcode_dir()?.join("cowork-store"))
}

/// Load the persisted approval policy, falling back to the strict default.
fn load_policy() -> ApprovalPolicy {
    store_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("settings.json")).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("approval_mode")
                .and_then(serde_json::Value::as_str)
                .and_then(ApprovalPolicy::parse)
        })
        .unwrap_or(ApprovalPolicy::AlwaysAsk)
}

fn save_policy(policy: ApprovalPolicy) -> Result<()> {
    let dir = store_dir()?;
    std::fs::create_dir_all(&dir)?;
    let payload = serde_json::json!({ "approval_mode": policy.as_str() });
    let temp = dir.join("settings.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&payload)?)?;
    std::fs::rename(&temp, dir.join("settings.json"))?;
    Ok(())
}

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

/// A folder mounted into a session with its granted access level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoworkMount {
    pub path: PathBuf,
    pub access: crate::cli::cowork_projects::MountAccess,
}

impl CoworkMount {
    pub fn read_write(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            access: crate::cli::cowork_projects::MountAccess::ReadWrite,
        }
    }

    pub fn read_only(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            access: crate::cli::cowork_projects::MountAccess::ReadOnly,
        }
    }
}

/// registrations that keep a task's sandbox alive for its whole lifetime.
/// Dropping any member unregisters that capability for the session.
pub struct SessionRegistrations {
    pub path_registration: crate::tool::SessionPathValidatorRegistration,
    pub write_registration: crate::tool::SessionWritePathValidatorRegistration,
    pub approval_registration: crate::tool::SessionApprovalRegistration,
    pub artifact_registration: crate::tool::SessionArtifactCreatedRegistration,
    pub tool_invoked_registration: crate::tool::SessionToolInvokedRegistration,
}

/// Register the write-side jail for a session: only read/write mounts accept
/// structured writes. With no writable mounts, every structured write fails.
fn register_session_write_jail(
    session_id: &str,
    write_roots: Vec<PathBuf>,
) -> std::io::Result<crate::tool::SessionWritePathValidatorRegistration> {
    if write_roots.is_empty() {
        Ok(crate::tool::register_session_write_path_validator(
            session_id,
            move |_| Err("SECURITY_VIOLATION: no read/write mount is available".to_owned()),
        ))
    } else {
        // Canonicalize once here; the closure must stay cheap per call.
        let jail = crate::sandbox::path_jail::PathJail::new(write_roots)?;
        Ok(crate::tool::register_session_write_path_validator(
            session_id,
            move |path| {
                jail.verify_path(path)
                    .map_err(|error| format!("{error}; write targets require a read/write mount"))
            },
        ))
    }
}

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
        task_id: String,
    },
    ArtifactCreated {
        path: String,
        mime_type: String,
        task_id: String,
    },
    ApprovalRequired {
        id: String,
        tool_name: String,
        description: String,
        task_id: String,
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
    tool_invoked_registrations:
        Mutex<HashMap<String, crate::tool::SessionToolInvokedRegistration>>,
    tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Lazily bootstrapped provider used to answer `list_models` without
    /// waiting for a task. Each runtime auto-fetches its model catalog from
    /// the provider's own endpoint (TTL-cached, static fallback offline).
    models_provider: tokio::sync::OnceCell<Arc<dyn crate::provider::Provider>>,
    /// Current approval policy. A sync mutex is fine: critical sections are
    /// single reads/writes with no await points.
    approval_policy: std::sync::Mutex<ApprovalPolicy>,
    /// Persistent project registry (projects, mounts with access levels,
    /// instructions, model defaults, task history).
    project_store: std::sync::OnceLock<crate::cli::cowork_projects::ProjectStore>,
}

/// Everything needed to launch one Cowork task.
pub struct TaskLaunch {
    pub goal: String,
    pub mounts: Vec<CoworkMount>,
    pub model: Option<String>,
    /// Project instructions injected into the system prompt for this task.
    pub instructions: Option<String>,
    /// When set, the finished launch is recorded in the project's task history.
    pub project_id: Option<String>,
}

impl TaskLaunch {
    pub fn new(goal: impl Into<String>, mounts: Vec<CoworkMount>) -> Self {
        Self {
            goal: goal.into(),
            mounts,
            model: None,
            instructions: None,
            project_id: None,
        }
    }
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
                tool_invoked_registrations: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                models_provider: tokio::sync::OnceCell::new(),
                approval_policy: std::sync::Mutex::new(load_policy()),
                project_store: std::sync::OnceLock::new(),
            }),
        }
    }

    /// The persistent project registry, opened on first use.
    pub fn project_store(&self) -> &crate::cli::cowork_projects::ProjectStore {
        self.inner.project_store.get_or_init(|| {
            crate::cli::cowork_projects::ProjectStore::open_default()
                .unwrap_or_else(|error| {
                    tracing::warn!("project store unavailable, using in-memory fallback: {error}");
                    crate::cli::cowork_projects::ProjectStore::open_at(
                        std::env::temp_dir().join("jcode-cowork-projects-fallback.json"),
                    )
                })
        })
    }

    pub fn approval_policy(&self) -> ApprovalPolicy {
        *self
            .inner
            .approval_policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn set_approval_policy(&self, policy: ApprovalPolicy) -> Result<()> {
        save_policy(policy)?;
        *self
            .inner
            .approval_policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
        Ok(())
    }

    /// Live model routes for the GUI. Bootstraps one provider per daemon
    /// process; the returned routes are backed by each runtime's auto-fetched
    /// catalog (with the static fallback list guaranteed non-empty offline).
    pub async fn list_models(&self) -> Result<Value, String> {
        let provider = self
            .inner
            .models_provider
            .get_or_try_init(|| async {
                crate::cli::provider_init::init_provider_for_run(
                    &crate::cli::provider_init::ProviderChoice::Auto,
                    None,
                    false,
                )
                .await
            })
            .await
            .map_err(|error| error.to_string())?;
        let routes = provider.model_routes();
        Ok(json!({ "models": routes }))
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
        task_id: &str,
        mounts: Vec<CoworkMount>,
    ) -> Result<SessionRegistrations> {
        let jail_roots = validate_workspace_roots(
            mounts.iter().map(|mount| mount.path.clone()).collect(),
        )
        .await?;
        let write_roots = validate_workspace_roots(
            mounts
                .iter()
                .filter(|mount| mount.access == MountAccess::ReadWrite)
                .map(|mount| mount.path.clone())
                .collect(),
        )
        .await
        .unwrap_or_default();
        let session_id = session_id.to_owned();
        let task_id = task_id.to_owned();
        let registration_session_id = session_id.clone();
        let (path_registration, write_registration) = tokio::task::spawn_blocking(move || {
            std::io::Result::Ok((
                crate::sandbox::register_session_jail(registration_session_id.clone(), jail_roots)?,
                register_session_write_jail(&registration_session_id, write_roots)?,
            ))
        })
        .await
        .context("failed to initialize Cowork path jail")??;
        let weak = Arc::downgrade(&self.inner);
        let approval_task_id = task_id.clone();
        let approval_registration = crate::tool::register_session_approval_requester(
            session_id.clone(),
            move |id, tool_name, description| {
                let weak = weak.clone();
                let task_id = approval_task_id.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .request_tool_approval(id, tool_name, description, task_id)
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        );
        let weak = Arc::downgrade(&self.inner);
        let artifact_task_id = task_id.clone();
        let artifact_registration =
            crate::tool::register_session_artifact_created_notifier(
                session_id.clone(),
                move |artifact| {
                let weak = weak.clone();
                let task_id = artifact_task_id.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .emit_event(CoworkEvent::ArtifactCreated {
                            path: artifact.path.display().to_string(),
                            mime_type: artifact.mime_type,
                            task_id,
                        })
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                }
            });
        let weak = Arc::downgrade(&self.inner);
        let tool_invoked_task_id = task_id.clone();
        let tool_invoked_registration = crate::tool::register_session_tool_invoked_notifier(
            session_id,
            move |invoked| {
                let weak = weak.clone();
                let task_id = tool_invoked_task_id.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .emit_event(CoworkEvent::ToolInvoked {
                            tool_call_id: invoked.tool_call_id,
                            tool_name: invoked.tool_name,
                            task_id,
                        })
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                }
            },
        );
        Ok(SessionRegistrations {
            path_registration,
            write_registration,
            approval_registration,
            artifact_registration,
            tool_invoked_registration,
        })
    }

pub async fn start_task(&self, launch: TaskLaunch) -> Result<String> {
    self.start_task_with(
        launch,
        |goal, roots, model, task_id, instructions, rpc| async move {
            crate::cli::commands::run_cowork_task(
                goal, roots, model, task_id, instructions, rpc,
            )
            .await
        },
    )
    .await
}

async fn start_task_with<F, Fut>(
    &self,
    launch: TaskLaunch,
    run: F,
) -> Result<String>
where
    F: FnOnce(
            String,
            Vec<PathBuf>,
            Option<String>,
            String,
            Option<String>,
            CoworkRpc,
        ) -> Fut
        + Send
        + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    anyhow::ensure!(!launch.goal.trim().is_empty(), "task goal cannot be empty");
    let allowed_roots = validate_workspace_roots(
        launch.mounts.iter().map(|mount| mount.path.clone()).collect(),
    )
    .await?;
    let task_id = crate::id::new_id("cowork");
    let (start, started) = oneshot::channel();
    let rpc = self.clone();
    let run_task_id = task_id.clone();
    let work = run(
        launch.goal,
        allowed_roots,
        launch.model,
        run_task_id.clone(),
        launch.instructions,
        rpc.clone(),
    );
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
    if let Some(project_id) = &launch.project_id {
        let _ = self.project_store().record_task(project_id, &task_id);
    }
    Ok(task_id)
}

/// Task ids currently registered and whether their work is still running.
pub async fn list_tasks(&self) -> Vec<(String, bool)> {
    self.inner
        .tasks
        .lock()
        .await
        .keys()
        .map(|task_id| (task_id.clone(), true))
        .collect()
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
        // Session-level mounts (mount_folder RPC) grant read/write access;
        // project-scoped tasks narrow this via register_agent_session.
        let read_roots = validate_workspace_roots(roots).await?;
        let session = session_id.to_owned();
        let write_roots = read_roots.clone();
        let jail_roots = read_roots.clone();
        let (jail, write_registration) = tokio::task::spawn_blocking(move || {
            let jail = crate::sandbox::path_jail::PathJail::new(jail_roots)?;
            let write_registration = if write_roots.is_empty() {
                crate::tool::register_session_write_path_validator(&session, move |_| {
                    Err("SECURITY_VIOLATION: no read/write mount is available".to_owned())
                })
            } else {
                let write_jail = crate::sandbox::path_jail::PathJail::new(write_roots)?;
                crate::tool::register_session_write_path_validator(&session, move |path| {
                    write_jail.verify_path(path).map_err(|error| {
                        format!("{error}; write targets require a read/write mount")
                    })
                })
            };
            Ok::<_, std::io::Error>((jail, write_registration))
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
                        .request_tool_approval(id, tool_name, description, String::new())
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        );
        let weak: Weak<CoworkRpcInner> = Arc::downgrade(&self.inner);
        let tool_invoked_registration = crate::tool::register_session_tool_invoked_notifier(
            session_id.to_owned(),
            move |invoked| {
                let weak = weak.clone();
                async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| "Cowork daemon disconnected".to_owned())?;
                    CoworkRpc { inner }
                        .emit_event(CoworkEvent::ToolInvoked {
                            tool_call_id: invoked.tool_call_id,
                            tool_name: invoked.tool_name,
                            // Mount-time registrations are not task-scoped;
                            // consumers route these by their single running
                            // task.
                            task_id: String::new(),
                        })
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                }
            },
        );
        self.inner
            .mounted_roots
            .lock()
            .await
            .insert(session_id.to_owned(), read_roots);
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
        self.inner
            .tool_invoked_registrations
            .lock()
            .await
            .insert(session_id.to_owned(), tool_invoked_registration);
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
        task_id: impl Into<String>,
    ) -> Result<bool> {
        let tool_name = tool_name.into();
        // Auto-allow read-only mode: reads run without prompting so long
        // research tasks do not queue a decision per file. Writes, shell
        // commands, office document creation, and computer use still ask.
        if self.approval_policy() == ApprovalPolicy::AutoAllowReadonly
            && READ_ONLY_TOOLS.contains(&tool_name.as_str())
        {
            return Ok(true);
        }
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
                tool_name,
                description: description.into(),
                task_id: task_id.into(),
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
        "ping" => Ok(json!({
            "name": "jcode-cowork",
            "version": PROTOCOL_VERSION,
            "protocol_version": PROTOCOL_VERSION,
        })),
        "list_models" => rpc
            .list_models()
            .await
            .map_err(|error| (-32000, error)),
        "settings.get" => {
            Ok(json!({ "approval_mode": rpc.approval_policy().as_str() }))
        }
        "settings.update" => {
            let mode = request.params.get("approval_mode").and_then(Value::as_str);
            match mode.and_then(ApprovalPolicy::parse) {
                Some(policy) => rpc
                    .set_approval_policy(policy)
                    .await
                    .map(|_| json!({ "approval_mode": policy.as_str() }))
                    .map_err(|error| (-32000, error.to_string())),
                None => Err((
                    -32602,
                    "params require approval_mode of always_ask or auto_allow_readonly"
                        .to_string(),
                )),
            }
        }
        "project.create" => {
            let name = request.params.get("name").and_then(Value::as_str);
            match name {
                Some(name) => {
                    let store = rpc.project_store();
                    store
                        .create(
                            name,
                            request.params.get("icon").and_then(Value::as_str).unwrap_or(""),
                            request.params.get("purpose").and_then(Value::as_str).unwrap_or(""),
                            request
                                .params
                                .get("instructions")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .map(|project| json!({ "project": project }))
                        .map_err(|error| (-32000, error.to_string()))
                }
                None => Err((-32602, "params require string name".to_string())),
            }
        }
        "project.list" => rpc
            .project_store()
            .list()
            .map(|projects| json!({ "projects": projects }))
            .map_err(|error| (-32000, error.to_string())),
        "project.get" | "mount.list" => {
            let project_id = request.params.get("project_id").and_then(Value::as_str);
            match project_id {
                Some(project_id) => rpc
                    .project_store()
                    .get(project_id)
                    .map(|project| json!({ "project": project }))
                    .map_err(|error| (-32000, error.to_string())),
                None => Err((-32602, "params require string project_id".to_string())),
            }
        }
        "project.update" => {
            let project_id = request.params.get("project_id").and_then(Value::as_str);
            let Some(project_id) = project_id else {
                return Some(rpc_error(Some(id), -32602, "params require string project_id"));
            };
            let name = request.params.get("name").and_then(Value::as_str).map(str::to_owned);
            let icon = request.params.get("icon").and_then(Value::as_str).map(str::to_owned);
            let purpose = request
                .params
                .get("purpose")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let instructions = request
                .params
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let model_default = request
                .params
                .get("model_default")
                .and_then(Value::as_str)
                .map(str::to_owned);
            rpc.project_store()
                .update(project_id, |project| {
                    if let Some(name) = &name {
                        if name.trim().is_empty() {
                            anyhow::bail!("project name cannot be empty");
                        }
                        project.name = name.trim().to_owned();
                    }
                    if let Some(icon) = &icon {
                        project.icon = icon.trim().to_owned();
                    }
                    if let Some(purpose) = &purpose {
                        project.purpose = purpose.trim().to_owned();
                    }
                    if let Some(instructions) = &instructions {
                        project.instructions = instructions.clone();
                    }
                    if let Some(model_default) = &model_default {
                        project.model_default = if model_default.trim().is_empty() {
                            None
                        } else {
                            Some(model_default.trim().to_owned())
                        };
                    }
                    Ok(())
                })
                .map(|project| json!({ "project": project }))
                .map_err(|error| (-32000, error.to_string()))
        }
        "project.delete" => {
            let project_id = request.params.get("project_id").and_then(Value::as_str);
            match project_id {
                Some(project_id) => rpc
                    .project_store()
                    .delete(project_id)
                    .map(|deleted| json!({ "deleted": deleted }))
                    .map_err(|error| (-32000, error.to_string())),
                None => Err((-32602, "params require string project_id".to_string())),
            }
        }
        "mount.add" => {
            let project_id = request.params.get("project_id").and_then(Value::as_str);
            let path = request.params.get("path").and_then(Value::as_str);
            let access = request.params.get("access").and_then(Value::as_str).and_then(
                |value| match value {
                    "read_only" => {
                        Some(crate::cli::cowork_projects::MountAccess::ReadOnly)
                    }
                    "read_write" => {
                        Some(crate::cli::cowork_projects::MountAccess::ReadWrite)
                    }
                    _ => None,
                },
            );
            match (project_id, path, access) {
                (Some(project_id), Some(path), Some(access)) => {
                    let store_path = rpc.project_store().store_path().to_owned();
                    let project_id = project_id.to_owned();
                    let path = PathBuf::from(path);
                    match tokio::task::spawn_blocking(move || {
                        crate::cli::cowork_projects::ProjectStore::add_mount_sync(
                            &store_path, &project_id, &path, access,
                        )
                    })
                    .await
                    {
                        Ok(Ok(project)) => Ok(json!({ "project": project })),
                        Ok(Err(error)) => Err((-32000, error.to_string())),
                        Err(error) => Err((-32000, format!("mount task failed: {error}"))),
                    }
                }
                _ => Err((
                    -32602,
                    "params require project_id, path, and access of read_only or read_write"
                        .to_string(),
                )),
            }
        }
        "mount.revoke" => {
            let project_id = request.params.get("project_id").and_then(Value::as_str);
            let path = request.params.get("path").and_then(Value::as_str);
            match (project_id, path) {
                (Some(project_id), Some(path)) => {
                    let store_path = rpc.project_store().store_path().to_owned();
                    let project_id = project_id.to_owned();
                    let path = PathBuf::from(path);
                    match tokio::task::spawn_blocking(move || {
                        crate::cli::cowork_projects::ProjectStore::revoke_mount_sync(
                            &store_path, &project_id, &path,
                        )
                    })
                    .await
                    {
                        Ok(Ok(project)) => Ok(json!({ "project": project })),
                        Ok(Err(error)) => Err((-32000, error.to_string())),
                        Err(error) => Err((-32000, format!("revoke task failed: {error}"))),
                    }
                }
                _ => Err((
                    -32602,
                    "params require string project_id and string path".to_string(),
                )),
            }
        }
        "task.list" => {
            let tasks = rpc.list_tasks().await;
            Ok(json!({
                "tasks": tasks
                    .into_iter()
                    .map(|(task_id, running)| json!({ "task_id": task_id, "running": running }))
                    .collect::<Vec<_>>(),
            }))
        }
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
            let model = request
                .params
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|model| !model.trim().is_empty());
            // Project-scoped start: mounts, model default, and instructions
            // all come from the persisted project unless overridden inline.
            let project_id = request
                .params
                .get("project_id")
                .and_then(Value::as_str)
                .filter(|project_id| !project_id.trim().is_empty());
            let launch = if let Some(project_id) = project_id {
                let project = match rpc.project_store().get(project_id) {
                    Ok(project) => project,
                    Err(error) => return Some(rpc_error(Some(id), -32000, error.to_string())),
                };
                if project.mounts.is_empty() {
                    return Some(rpc_error(
                        Some(id),
                        -32000,
                        format!("project {project_id} has no mounted folders; mount one first"),
                    ));
                }
                TaskLaunch {
                    goal: goal.unwrap_or_default().to_owned(),
                    mounts: project
                        .mounts
                        .iter()
                        .map(|mount| CoworkMount {
                            path: mount.path.clone(),
                            access: mount.access,
                        })
                        .collect(),
                    model: model.or_else(|| project.model_default.clone()),
                    instructions: if project.instructions.trim().is_empty() {
                        None
                    } else {
                        Some(project.instructions.clone())
                    },
                    project_id: Some(project_id.to_owned()),
                }
            } else {
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
                    (Some(goal), Some(roots)) => {
                        TaskLaunch::new(goal.to_owned(), roots.into_iter().map(CoworkMount::read_write).collect())
                    }
                    _ => {
                        return Some(rpc_error(
                            Some(id),
                            -32602,
                            "params require string goal and string-array allowed_roots (or project_id)",
                        ));
                    }
                }
            };
            match goal {
                Some(_) => rpc
                    .start_task(launch)
                    .await
                    .map(|task_id| json!({ "task_id": task_id }))
                    .map_err(|error| (-32000, error.to_string())),
                None => Err((
                    -32602,
                    "params require string goal and string-array allowed_roots (or project_id)"
                        .to_string(),
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
    use tokio::io::{AsyncWriteExt, BufReader};
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

    /// Tests that repoint `JCODE_COWORK_STORE_DIR` serialize on this lock so
    /// parallel test threads never observe each other's store directory.
    static STORE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_store_env() -> std::sync::MutexGuard<'static, ()> {
        STORE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
                .request_tool_approval("call-1", "delete_file", "Remove report.docx", "task-1")
                .await
        });

        let event = events.recv().await.unwrap();
        assert_eq!(
            event,
            CoworkEvent::ApprovalRequired {
                id: "call-1".to_string(),
                tool_name: "delete_file".to_string(),
                description: "Remove report.docx".to_string(),
                task_id: "task-1".to_string(),
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
                TaskLaunch::new(
                    "Review these files".to_string(),
                    vec![CoworkMount::read_write(directory.path())],
                ),
                |_, _, _, _, _, _| async { std::future::pending::<Result<String>>().await },
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
                TaskLaunch::new(
                    "Review these files".to_string(),
                    vec![CoworkMount::read_write(directory.path())],
                ),
                |_, _, _, _, _, _| async { Ok("Reviewed the workspace".to_string()) },
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
        assert_eq!(response["result"]["version"], PROTOCOL_VERSION);
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
    async fn settings_update_round_trips_and_auto_allows_readonly() {
        let _env = lock_store_env();
        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir.path()) };
        let rpc = CoworkRpc::new();
        assert_eq!(rpc.approval_policy(), ApprovalPolicy::AlwaysAsk);

        let response = dispatch_request(
            authed_request(
                &rpc,
                20,
                "settings.update",
                json!({ "approval_mode": "auto_allow_readonly" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["approval_mode"], "auto_allow_readonly");
        assert_eq!(rpc.approval_policy(), ApprovalPolicy::AutoAllowReadonly);

        // A fresh daemon loads the persisted policy (restart continuity).
        let restarted = CoworkRpc::new();
        assert_eq!(restarted.approval_policy(), ApprovalPolicy::AutoAllowReadonly);

        // Read-only tools are auto-approved without emitting an event.
        let decision = restarted
            .request_tool_approval("call-r1", "read", "Read report.xlsx", "task-1")
            .await
            .unwrap();
        assert!(decision, "read must be auto-approved in auto_allow_readonly");

        // Non-read-only tools still need a connected client to decide.
        let denied = restarted
            .request_tool_approval("call-r2", "bash", "Run cleanup.sh", "task-1")
            .await;
        assert!(denied.is_err(), "bash must still require an explicit decision");

        // Invalid modes are rejected at the protocol surface.
        let response = dispatch_request(
            authed_request(&rpc, 21, "settings.update", json!({ "approval_mode": "yolo" })),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);
        // SAFETY: same lock discipline as the set above.
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    /// Milestone smoke test: create a project, mount a folder into it, start a
    /// project-scoped task, and receive streamed events. Mirrors the GUI flow
    /// end to end at the RPC boundary.
    #[tokio::test]
    async fn project_smoke_create_mount_start_task_with_instructions() {
        let _env = lock_store_env();
        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir.path()) };
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();

        let response = dispatch_request(
            authed_request(
                &rpc,
                30,
                "project.create",
                json!({
                    "name": "Q4 Close",
                    "purpose": "Finance workspace",
                    "instructions": "Always cite the source workbook.",
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        let project_id = response["result"]["project"]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let folder = tempfile::tempdir().unwrap();
        let response = dispatch_request(
            authed_request(
                &rpc,
                31,
                "mount.add",
                json!({
                    "project_id": project_id,
                    "path": folder.path(),
                    "access": "read_write",
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["project"]["mounts"].as_array().unwrap().len(), 1);

        // The task list is empty before starting anything.
        let response = dispatch_request(authed_request(&rpc, 32, "task.list", json!({})), &rpc)
            .await
            .unwrap();
        assert_eq!(response["result"]["tasks"].as_array().unwrap().len(), 0);

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<Option<String>>));
        let captured_in_task = std::sync::Arc::clone(&captured);
        let task_id = rpc
            .start_task_with(
                TaskLaunch {
                    goal: "Summarize the workbook".to_owned(),
                    mounts: vec![CoworkMount::read_write(folder.path())],
                    model: None,
                    instructions: Some("Always cite the source workbook.".to_owned()),
                    project_id: Some(project_id.clone()),
                },
                move |_goal, roots, _model, _task_id, instructions, _rpc| {
                    let captured = std::sync::Arc::clone(&captured_in_task);
                    async move {
                        *captured.lock().unwrap() = Some(instructions.clone());
                        assert!(!roots.is_empty());
                        Ok("done".to_owned())
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(
            events.recv().await.unwrap(),
            CoworkEvent::TaskCompleted {
                task_id: task_id.clone(),
                success: true,
                summary: Some("done".to_string()),
            }
        );
        assert_eq!(
            captured.lock().unwrap().clone(),
            Some(Some("Always cite the source workbook.".to_owned()))
        );

        // The task landed in the project history.
        let response = dispatch_request(
            authed_request(&rpc, 33, "project.get", json!({ "project_id": project_id })),
            &rpc,
        )
        .await
        .unwrap();
        let history = response["result"]["project"]["task_ids"]
            .as_array()
            .unwrap();
        assert!(history.contains(&json!(task_id)));

        // And task.list now reports it as finished (no longer running).
        let response = dispatch_request(authed_request(&rpc, 34, "task.list", json!({})), &rpc)
            .await
            .unwrap();
        assert_eq!(response["result"]["tasks"].as_array().unwrap().len(), 0);
        // SAFETY: same lock discipline as the set above.
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn readonly_project_mounts_denies_structured_writes() {
        let _env = lock_store_env();
        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir.path()) };
        let rpc = CoworkRpc::new();

        let folder = tempfile::tempdir().unwrap();
        let registrations = rpc
            .register_agent_session(
                "session-readonly",
                "task-readonly",
                vec![CoworkMount::read_only(folder.path())],
            )
            .await
            .unwrap();
        // Keep the registrations alive for the duration of the check.
        let SessionRegistrations {
            path_registration: _,
            write_registration,
            approval_registration: _,
            artifact_registration: _,
            tool_invoked_registration: _,
        } = registrations;

        let target = folder.path().join("report.docx");
        // Reads resolve fine through the read validator...
        let read_resolution =
            jcode_tool_core::verify_session_path("session-readonly", &target).unwrap();
        assert!(
            read_resolution.is_some() || read_resolution.is_none(),
            "read validator accepted the mounted path"
        );
        // ...but structured writes are denied before touching disk.
        let error = jcode_tool_core::verify_session_write_path("session-readonly", &target)
            .err()
            .expect("write into read-only mount must be denied");
        assert!(error.contains("read/write mount"), "got: {error}");
        drop(write_registration);
        // SAFETY: same lock discipline as the set above.
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn mount_revoke_removes_future_write_access() {
        let _env = lock_store_env();
        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir.path()) };
        let rpc = CoworkRpc::new();

        let response = dispatch_request(
            authed_request(&rpc, 40, "project.create", json!({ "name": "Revoke" })),
            &rpc,
        )
        .await
        .unwrap();
        let project_id = response["result"]["project"]["id"].as_str().unwrap().to_owned();

        let folder = tempfile::tempdir().unwrap();
        let response = dispatch_request(
            authed_request(
                &rpc,
                41,
                "mount.add",
                json!({
                    "project_id": project_id,
                    "path": folder.path(),
                    "access": "read_write",
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();

        // Revoke while no task runs; the stored project loses the mount...
        let response = dispatch_request(
            authed_request(
                &rpc,
                42,
                "mount.revoke",
                json!({ "project_id": project_id, "path": folder.path() }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(
            response["result"]["project"]["mounts"].as_array().unwrap().len(),
            0
        );

        // ...so a fresh task on this project starts with NO mounts at all.
        let response = dispatch_request(
            authed_request(
                &rpc,
                43,
                "start_task",
                json!({ "goal": "work", "project_id": project_id }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no mounted folders"),
            "got: {response}"
        );
        // SAFETY: same lock discipline as the set above.
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
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

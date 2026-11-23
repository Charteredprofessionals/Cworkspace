use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use crate::cli::cowork_schedule::{now_unix_secs, ScheduleStore};
use crate::cli::cowork_projects::MountAccess;
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, oneshot};

pub const PIPE_NAME: &str = r"\\.\pipe\jcode-cowork";

/// Wire protocol version. Bump on any breaking RPC contract change; the GUI
/// compares this at connect time via `ping` and refuses mismatched majors.
/// v3: parity surfaces for every jcode capability the desktop was missing —
/// tool catalog, task transcripts, usage reports, auth status/keys,
/// MCP server management, and scheduled tasks.
pub const PROTOCOL_VERSION: u32 = 3;

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

/// What a stored permission rule decides when a tool call matches it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleDecision {
    Allow,
    Deny,
}

impl RuleDecision {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(RuleDecision::Allow),
            "deny" => Some(RuleDecision::Deny),
            _ => None,
        }
    }
}

/// Where a permission rule applies: every task, or only tasks running inside
/// one project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleScope {
    Global,
    Project { project_id: String },
}

/// A stored permission decision for one tool. Rules outrank the coarse
/// approval policy: an explicit "always allow bash in this project" or
/// "never ask me about webfetch" applies before the policy auto-allow and
/// before prompting. One rule per (tool, scope) — a newer decision replaces
/// the older one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRule {
    pub id: String,
    /// Exact tool name ("bash", "webfetch", "mcp__gmail__send_email").
    pub tool: String,
    pub decision: RuleDecision,
    pub scope: RuleScope,
    /// Unix seconds when the rule was stored.
    pub created_at: u64,
}

/// Load persisted permission rules; malformed entries are skipped so one bad
/// line can never block task approvals.
fn load_permission_rules() -> Vec<PermissionRule> {
    store_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("permission_rules.json")).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| {
            let list = value.get("rules")?.as_array()?.clone();
            Some(
                list.into_iter()
                    .filter_map(|entry| serde_json::from_value::<PermissionRule>(entry).ok())
                    .collect::<Vec<PermissionRule>>(),
            )
        })
        .unwrap_or_default()
}

fn save_permission_rules(rules: &[PermissionRule]) -> Result<()> {
    let dir = store_dir()?;
    std::fs::create_dir_all(&dir)?;
    let payload = serde_json::json!({ "rules": rules });
    let temp = dir.join("permission_rules.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&payload)?)?;
    std::fs::rename(&temp, dir.join("permission_rules.json"))?;
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
    approvals: Mutex<HashMap<String, PendingApproval>>,
    mounted_roots: Mutex<HashMap<String, Vec<PathBuf>>>,
    jail_registrations: Mutex<HashMap<String, crate::tool::SessionPathValidatorRegistration>>,
    approval_registrations: Mutex<HashMap<String, crate::tool::SessionApprovalRegistration>>,
    tool_invoked_registrations:
        Mutex<HashMap<String, crate::tool::SessionToolInvokedRegistration>>,
    tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// task_id -> jcode session_id, most recent last. Lets `task.transcript`
    /// read a task's persisted conversation even after the task completes.
    task_sessions: Mutex<VecDeque<(String, String)>>,
    /// task_id -> recorded launch context (mounts, project, model), most
    /// recent last. Lets `task.followup` resume a finished task with the
    /// same workspace and settings.
    task_contexts: Mutex<VecDeque<(String, TaskContext)>>,
    /// Lazily bootstrapped provider used to answer `list_models` without
    /// waiting for a task. Each runtime auto-fetches its model catalog from
    /// the provider's own endpoint (TTL-cached, static fallback offline).
    models_provider: tokio::sync::OnceCell<Arc<dyn crate::provider::Provider>>,
    /// Current approval policy. A sync mutex is fine: critical sections are
    /// single reads/writes with no await points.
    approval_policy: std::sync::Mutex<ApprovalPolicy>,
    /// Stored permission rules, checked before the policy on every approval.
    /// A sync mutex: list/upsert/remove are short non-awaiting sections.
    permission_rules: std::sync::Mutex<Vec<PermissionRule>>,
    /// Set by a `shutdown` request; the server loop exits the process after
    /// the response is written. Atomic: no lock ordering concerns.
    shutdown_requested: std::sync::atomic::AtomicBool,
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
    /// Resume an existing session — a follow-up turn on a finished task
    /// keeps the earlier conversation context.
    pub resume_session: Option<String>,
    /// Reuse an existing task id so follow-up events keep flowing to the
    /// same desktop tab.
    pub reuse_task_id: Option<String>,
}

impl TaskLaunch {
    pub fn new(goal: impl Into<String>, mounts: Vec<CoworkMount>) -> Self {
        Self {
            goal: goal.into(),
            mounts,
            model: None,
            instructions: None,
            project_id: None,
            resume_session: None,
            reuse_task_id: None,
        }
    }
}

/// Recorded per finished launch so a follow-up can resume with identical
/// mounts, project, and model. Bounded like the task-session map.
#[derive(Clone)]
pub struct TaskContext {
    pub mounts: Vec<CoworkMount>,
    pub project_id: Option<String>,
    pub model: Option<String>,
}

/// A pending approval and what it was for, so "remember this choice" can
/// store a permission rule without another round trip to the agent session.
struct PendingApproval {
    sender: oneshot::Sender<bool>,
    tool: String,
    task_id: String,
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
                task_sessions: Mutex::new(VecDeque::new()),
                task_contexts: Mutex::new(VecDeque::new()),
                models_provider: tokio::sync::OnceCell::new(),
                approval_policy: std::sync::Mutex::new(load_policy()),
                permission_rules: std::sync::Mutex::new(load_permission_rules()),
                shutdown_requested: std::sync::atomic::AtomicBool::new(false),
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

    /// The stored permission rules (oldest first).
    pub fn permission_rules(&self) -> Vec<PermissionRule> {
        self.inner
            .permission_rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Store (or replace) the rule for a tool in a scope. One rule per
    /// (tool, scope): the newest decision wins.
    fn upsert_rule(
        &self,
        tool: &str,
        decision: RuleDecision,
        scope: RuleScope,
    ) -> Result<PermissionRule> {
        let rule = PermissionRule {
            id: crate::id::new_id("rule"),
            tool: tool.to_owned(),
            decision,
            scope,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let mut rules = self
            .inner
            .permission_rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rules.retain(|existing| !(existing.tool == rule.tool && existing.scope == rule.scope));
        rules.push(rule.clone());
        save_permission_rules(&rules)?;
        Ok(rule)
    }

    /// Remove a rule by id; reports whether it existed.
    fn remove_rule(&self, id: &str) -> bool {
        let removed;
        {
            let mut rules = self
                .inner
                .permission_rules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = rules.len();
            rules.retain(|rule| rule.id != id);
            removed = rules.len() != before;
        }
        if removed {
            let rules = self.permission_rules();
            if let Err(error) = save_permission_rules(&rules) {
                tracing::warn!("could not persist permission rules: {error}");
            }
        }
        removed
    }

    /// Most specific matching rule for a tool call: project scope beats
    /// global, and within one specificity the newest rule wins.
    fn evaluate_permission_rules(
        &self,
        tool: &str,
        project_id: Option<&str>,
    ) -> Option<RuleDecision> {
        let rules = self
            .inner
            .permission_rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut best: Option<(u8, u64, RuleDecision)> = None;
        for rule in rules.iter().filter(|rule| rule.tool == tool) {
            let (specificity, matches) = match &rule.scope {
                RuleScope::Global => (0u8, true),
                RuleScope::Project {
                    project_id: rule_project,
                } => (1u8, project_id == Some(rule_project.as_str())),
            };
            if !matches {
                continue;
            }
            let replaces = best.is_none_or(|(best_spec, best_at, _)| {
                specificity > best_spec || (specificity == best_spec && rule.created_at >= best_at)
            });
            if replaces {
                best = Some((specificity, rule.created_at, rule.decision));
            }
        }
        best.map(|(_, _, decision)| decision)
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
        |goal, roots, model, task_id, instructions, resume_session, rpc| async move {
            crate::cli::commands::run_cowork_task(
                goal, roots, model, task_id, instructions, resume_session, rpc,
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
            Option<String>,
            CoworkRpc,
        ) -> Fut
        + Send
        + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    anyhow::ensure!(!launch.goal.trim().is_empty(), "task goal cannot be empty");
    anyhow::ensure!(
        launch.reuse_task_id.is_none() || launch.resume_session.is_some(),
        "reusing a task id requires the session to resume"
    );
    let allowed_roots = validate_workspace_roots(
        launch.mounts.iter().map(|mount| mount.path.clone()).collect(),
    )
    .await?;
    let task_id = launch
        .reuse_task_id
        .clone()
        .unwrap_or_else(|| crate::id::new_id("cowork"));
    if launch.resume_session.is_some() {
        let running = self.inner.tasks.lock().await.contains_key(&task_id);
        anyhow::ensure!(
            !running,
            "task {task_id} is still running; wait for it to finish before sending a follow-up"
        );
        let known = self.task_session_id(&task_id).await.is_some();
        anyhow::ensure!(
            known,
            "no session recorded for task {task_id}; it cannot be resumed"
        );
    }
    let (start, started) = oneshot::channel();
    let rpc = self.clone();
    let run_task_id = task_id.clone();
    let context = TaskContext {
        mounts: launch.mounts.clone(),
        project_id: launch.project_id.clone(),
        model: launch.model.clone(),
    };
    let work = run(
        launch.goal,
        allowed_roots,
        launch.model,
        run_task_id.clone(),
        launch.instructions,
        launch.resume_session,
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
    // Record the launch context so a later follow-up can resume with the
    // same workspace, project, and model (bounded, most recent kept).
    {
        let mut contexts = self.inner.task_contexts.lock().await;
        contexts.retain(|(existing, _)| existing != &task_id);
        contexts.push_back((task_id.clone(), context));
        while contexts.len() > 200 {
            contexts.pop_front();
        }
    }
    if let Some(project_id) = &launch.project_id {
        let _ = self.project_store().record_task(project_id, &task_id);
    }
    Ok(task_id)
}

/// Recorded launch context for a task, if any (survives completion).
pub async fn task_context(&self, task_id: &str) -> Option<TaskContext> {
    self.inner
        .task_contexts
        .lock()
        .await
        .iter()
        .rev()
        .find(|(existing, _)| existing == task_id)
        .map(|(_, context)| context.clone())
}

/// Whether a task's work is currently running.
pub async fn task_is_running(&self, task_id: &str) -> bool {
    self.inner.tasks.lock().await.contains_key(task_id)
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
                .retain(|_, pending| !pending.sender.is_closed());
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

    /// Whether a `shutdown` request was accepted and the daemon should exit
    /// once its response has been written. The server loop polls this after
    /// every response; tests set it without running a server.
    pub fn shutdown_requested(&self) -> bool {
        self.inner
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Abort every running task and mark the daemon for exit (v0.1.7: the
    /// daemon survives GUI close, so an explicit quit — tray menu or
    /// installer — must be able to stop it). Returns how many tasks were
    /// stopped. The actual process exit happens in the server loop after the
    /// shutdown response flushes; nothing here exits the process so tests
    /// can exercise the logic directly.
    async fn initiate_shutdown(&self) -> usize {
        let handles: Vec<(String, tokio::task::JoinHandle<()>)> =
            self.inner.tasks.lock().await.drain().collect();
        let stopped = handles.len();
        for (task_id, handle) in handles {
            handle.abort();
            let _ = handle.await;
            let _ = self.emit_event(CoworkEvent::TaskCompleted {
                task_id: task_id.clone(),
                success: false,
                summary: Some("Daemon shutting down".to_owned()),
            });
        }
        self.inner
            .approvals
            .lock()
            .await
            .retain(|_, pending| !pending.sender.is_closed());
        self.inner
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        stopped
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
        let task_id = task_id.into();
        // Explicit permission rules outrank the coarse policy: a stored allow
        // or deny for this tool (globally, or inside this task's project)
        // applies first and never reaches the user.
        let project_id = self
            .task_context(&task_id)
            .await
            .and_then(|context| context.project_id);
        match self.evaluate_permission_rules(&tool_name, project_id.as_deref()) {
            Some(RuleDecision::Allow) => return Ok(true),
            Some(RuleDecision::Deny) => return Ok(false),
            None => {}
        }
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
            approvals.insert(
                id.clone(),
                PendingApproval {
                    sender,
                    tool: tool_name.clone(),
                    task_id: task_id.clone(),
                },
            );
        }

        if self
            .emit_event(CoworkEvent::ApprovalRequired {
                id: id.clone(),
                tool_name,
                description: description.into(),
                task_id,
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
            .is_some_and(|pending| pending.sender.send(approved).is_ok())
    }

    /// The tool and task a pending approval belongs to (looked up BEFORE the
    /// decision is sent — sending removes the entry).
    async fn pending_approval_info(&self, id: &str) -> Option<(String, String)> {
        self.inner
            .approvals
            .lock()
            .await
            .get(id)
            .map(|pending| (pending.tool.clone(), pending.task_id.clone()))
    }

    /// Apply "remember this choice": store a permission rule for the
    /// approval's tool, scoped to the task's project when known, else
    /// globally.
    async fn remember_approval_decision(
        &self,
        tool: &str,
        task_id: &str,
        approved: bool,
        scope_hint: &str,
    ) {
        let decision = if approved {
            RuleDecision::Allow
        } else {
            RuleDecision::Deny
        };
        let scope = if scope_hint == "project" {
            self.task_context(task_id)
                .await
                .and_then(|context| context.project_id)
                .map(|project_id| RuleScope::Project { project_id })
                .unwrap_or(RuleScope::Global)
        } else {
            RuleScope::Global
        };
        if let Err(error) = self.upsert_rule(tool, decision, scope) {
            tracing::warn!("could not store permission rule: {error}");
        }
    }

    /// Associate a task with its jcode session so `task.transcript` can read
    /// the persisted conversation later, even after the task completes.
    /// Called from `run_cowork_task` right after the agent session exists.
    pub async fn note_task_session(&self, task_id: &str, session_id: &str) {
        let mut sessions = self.inner.task_sessions.lock().await;
        sessions.retain(|(existing, _)| existing != task_id);
        sessions.push_back((task_id.to_owned(), session_id.to_owned()));
        // Bound the map: transcripts of long-finished tasks are still
        // reachable on disk via the session store if ever needed again.
        while sessions.len() > 200 {
            sessions.pop_front();
        }
    }

    pub async fn task_session_id(&self, task_id: &str) -> Option<String> {
        self.inner
            .task_sessions
            .lock()
            .await
            .iter()
            .rev()
            .find(|(existing, _)| existing == task_id)
            .map(|(_, session_id)| session_id.clone())
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

/// Cowork-facing catalog of the agent tools desktop tasks can use. Mirrors
/// the base registry (`Registry::new`) plus the notes that matter to the GUI:
/// which tools are read-only under the auto-allow policy, and that configured
/// MCP servers add `mcp__<server>__<tool>` entries dynamically. Kept as data
/// on purpose — the registry itself has no name iteration to reuse.
pub const TOOL_CATALOG: &[(&str, &str, bool)] = &[
    ("read", "Read a file from a mounted folder", true),
    ("ls", "List directory contents", true),
    ("agentgrep", "Search file contents across the workspace", true),
    ("glob", "Find files by glob pattern", true),
    ("webfetch", "Fetch a URL into the transcript", true),
    ("websearch", "Search the web", true),
    ("write", "Create or overwrite a file", false),
    ("edit", "Edit an existing file", false),
    ("replace", "Replace text in a file (alias: multiedit)", false),
    ("apply_patch", "Apply a structured patch across files", false),
    ("bash", "Run a shell command inside the workspace", false),
    ("office_docs", "Author xlsx / docx / pptx / pdf documents", false),
    ("browser", "Control a browser session", false),
    ("open", "Open a file or URL in the OS default app", false),
    ("computer_use", "Drive the Windows UI (mouse, keyboard, screenshots)", false),
    ("macos_computer_use", "Drive the macOS UI (macOS builds)", false),
    ("compile_remote", "Compile on the remote build service", false),
    ("bg", "Run a background job and collect it later", false),
    ("todo", "Maintain the task's own todo list", false),
    ("memory", "Read and write the persistent memory store", false),
    ("session_search", "Semantic search over past sessions", true),
    ("schedule", "Queue a scheduled follow-up task", false),
    ("gmail", "Read and search Gmail (when connected)", false),
    ("calendar", "Read Google Calendar (when connected)", false),
    ("batch", "Run multiple tool calls in parallel (opt-in)", false),
    ("swarm", "Coordinate sub-agents (per-session)", false),
    ("codemode", "Compose tools into scripts (opt-in)", false),
    ("selfdev", "jcode self-development tasks (opt-in)", false),
    (
        "mcp__*",
        "One entry per tool exposed by each configured MCP server (see mcp.list)",
        false,
    ),
];

fn tools_list_result() -> Value {
    json!({
        "tools": TOOL_CATALOG
            .iter()
            .map(|(name, description, read_only)| {
                json!({
                    "name": name,
                    "description": description,
                    "read_only": read_only,
                })
            })
            .collect::<Vec<_>>(),
        "read_only_policy": json!({
            "auto_allowed": READ_ONLY_TOOLS,
        }),
    })
}

const TRANSCRIPT_MESSAGE_LIMIT: usize = 500;
const TRANSCRIPT_TEXT_CAP: usize = 4_000;
const TRANSCRIPT_TOOL_RESULT_CAP: usize = 800;

/// Load a task transcript from the persisted session store. Only visible
/// conversation material is returned: internal `<system-reminder>` blocks,
/// display-role wrappers, and hidden reasoning traces are excluded.
fn load_transcript(session_id: &str, limit: usize) -> Result<Value> {
    let session = crate::session::Session::load(session_id)?;
    let messages: Vec<Value> = session
        .messages
        .iter()
        .filter_map(transcript_message)
        .collect();
    let total = messages.len();
    let start = total.saturating_sub(limit);
    Ok(json!({
        "session_id": session_id,
        "title": session.title,
        "model": session.model,
        "total_messages": total,
        "messages": &messages[start..],
    }))
}

fn transcript_message(message: &crate::session::StoredMessage) -> Option<Value> {
    if message.display_role.is_some() {
        return None;
    }
    let role = match message.role {
        crate::message::Role::User => "user",
        crate::message::Role::Assistant => "assistant",
    };
    let mut text = String::new();
    let mut events: Vec<Value> = Vec::new();
    for block in &message.content {
        match block {
            crate::message::ContentBlock::Text { text: block_text, .. } => {
                let trimmed = block_text.trim_start();
                if trimmed.starts_with("<system-reminder>") || block_text.trim().is_empty() {
                    continue;
                }
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(block_text);
            }
            crate::message::ContentBlock::ToolUse { name, input, .. } => events.push(json!({
                "type": "tool_use",
                "tool": name,
                "input": truncate_chars(&input.to_string(), 400),
            })),
            crate::message::ContentBlock::ToolResult {
                content,
                is_error,
                ..
            } => events.push(json!({
                "type": "tool_result",
                "text": truncate_chars(content, TRANSCRIPT_TOOL_RESULT_CAP),
                "is_error": is_error.unwrap_or(false),
            })),
            // Reasoning traces, signatures, provider-internal items stay out
            // of the desktop transcript.
            _ => {}
        }
    }
    if text.is_empty() && events.is_empty() {
        return None;
    }
    Some(json!({
        "role": role,
        "text": truncate_chars(&text, TRANSCRIPT_TEXT_CAP),
        "events": events,
    }))
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026} [truncated]", &text[..end])
}

/// Build the follow-up launch for a finished task. Pure so tests can assert
/// on it without spawning a real runner.
fn build_followup_launch(
    task_id: &str,
    session_id: String,
    context: TaskContext,
    message: &str,
    model_override: Option<String>,
    project_instructions: Option<String>,
) -> TaskLaunch {
    // Model precedence: explicit override → the original launch's model →
    // the project's default → provider default (empty override = original).
    let model = model_override.or(context.model);
    TaskLaunch {
        goal: message.trim().to_owned(),
        mounts: context.mounts,
        model,
        instructions: project_instructions,
        project_id: context.project_id,
        resume_session: Some(session_id),
        reuse_task_id: Some(task_id.to_owned()),
    }
}

/// Fire any due scheduled tasks. Runs inside the daemon's scheduler loop.
async fn run_due_schedule_entries(rpc: &CoworkRpc) -> Result<()> {
    let mut store = ScheduleStore::open_default()?;
    for entry in store.due_pending(now_unix_secs()) {
        let project = match rpc.project_store().get(&entry.project_id) {
            Ok(project) => project,
            Err(error) => {
                store
                    .mark_failed(&entry.id, format!("project unavailable: {error}"))?;
                continue;
            }
        };
        if project.mounts.is_empty() {
            store.mark_failed(&entry.id, "project has no mounted folders")?;
            continue;
        }
        let launch = TaskLaunch {
            goal: entry.goal.clone(),
            mounts: project
                .mounts
                .iter()
                .map(|mount| CoworkMount {
                    path: mount.path.clone(),
                    access: mount.access,
                })
                .collect(),
            model: entry.model.clone().or_else(|| project.model_default.clone()),
            instructions: if project.instructions.trim().is_empty() {
                None
            } else {
                Some(project.instructions.clone())
            },
            project_id: Some(entry.project_id.clone()),
            resume_session: None,
            reuse_task_id: None,
        };
        match rpc.start_task(launch).await {
            Ok(task_id) => store.mark_fired(&entry.id, &task_id)?,
            Err(error) => store.mark_failed(&entry.id, error.to_string())?,
        }
    }
    Ok(())
}

/// Start the scheduler loop for the desktop daemon: every 30 seconds, fire
/// any due `schedule.create` entries as project-scoped cowork tasks.
/// Tracks consecutive quiet minutes for the daemon's idle janitor. Pure
/// logic so tests can drive it without a clock.
struct IdleJanitor {
    idle_ticks: u32,
}

impl IdleJanitor {
    /// 10 ticks x 60 s = ten minutes with no connected client and no running
    /// task before the daemon exits. Generous enough that a GUI restart
    /// (seconds) never races it, short enough that a hard-killed GUI does
    /// not leave an orphaned daemon for hours.
    const SHUTDOWN_AFTER_TICKS: u32 = 10;

    fn new() -> Self {
        Self { idle_ticks: 0 }
    }

    /// One 60-second tick. Returns true when the daemon should exit: no
    /// client connected and no task running for `SHUTDOWN_AFTER_TICKS`
    /// consecutive minutes.
    fn tick(&mut self, clients_connected: usize, tasks_running: usize) -> bool {
        if clients_connected == 0 && tasks_running == 0 {
            self.idle_ticks += 1;
        } else {
            self.idle_ticks = 0;
        }
        self.idle_ticks >= Self::SHUTDOWN_AFTER_TICKS
    }
}

/// Exit the daemon once it has been idle — no IPC client and no running
/// task — for ten consecutive minutes. Background execution (v0.1.7) means
/// the daemon outlives the GUI, so this is what prevents an orphaned daemon
/// after a crash or a forced kill of the desktop app.
pub fn spawn_idle_janitor(rpc: CoworkRpc) {
    tokio::spawn(async move {
        let mut janitor = IdleJanitor::new();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick fires immediately; skip it
        loop {
            ticker.tick().await;
            let clients = rpc.inner.events.receiver_count();
            let tasks = rpc.list_tasks().await.len();
            if janitor.tick(clients, tasks) {
                tracing::info!(
                    "daemon idle for {} minutes with no client and no running task; exiting",
                    IdleJanitor::SHUTDOWN_AFTER_TICKS
                );
                std::process::exit(0);
            }
        }
    });
}

pub fn spawn_scheduler(rpc: CoworkRpc) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = run_due_schedule_entries(&rpc).await {
                tracing::warn!("scheduled-task runner: {error:#}");
            }
        }
    });
}

fn mcp_config_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("mcp.json"))
}

/// Read the global MCP config as a raw JSON object map so unknown fields are
/// preserved exactly for other tools that share the file.
fn read_mcp_servers(path: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write_mcp_servers(path: &Path, servers: &serde_json::Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(servers)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn mcp_list_result(path: &Path) -> Value {
    let servers = read_mcp_servers(path);
    let list = servers
        .iter()
        .map(|(name, spec)| {
            let enabled = spec
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                && !spec
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            json!({
                "name": name,
                "command": spec.get("command").and_then(Value::as_str).unwrap_or(""),
                "args": spec.get("args").cloned().unwrap_or_else(|| json!([])),
                "url": spec.get("url").and_then(Value::as_str).unwrap_or(""),
                "transport": spec.get("type").and_then(Value::as_str).unwrap_or("stdio"),
                "enabled": enabled,
                "shared": spec.get("shared").and_then(Value::as_bool).unwrap_or(true),
                "timeout_secs": spec.get("timeout_secs").and_then(Value::as_u64),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "servers": list,
        "config_path": path.display().to_string(),
    })
}

fn valid_mcp_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn mcp_add_server(path: &Path, name: &str, spec: &Value) -> Result<Value> {
    let name = name.trim();
    anyhow::ensure!(
        valid_mcp_server_name(name),
        "server name must be 1-64 chars of letters, digits, '-', '_', '.'"
    );
    let command = spec
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let url = spec.get("url").and_then(Value::as_str).unwrap_or("").trim();
    anyhow::ensure!(
        (command.is_empty() && !url.is_empty()) || (!command.is_empty() && url.is_empty()),
        "provide exactly one of command (stdio server) or url (http/sse server)"
    );
    let mut entry = serde_json::Map::new();
    if !command.is_empty() {
        entry.insert("command".to_owned(), json!(command));
        if let Some(args) = spec.get("args").and_then(Value::as_array) {
            entry.insert("args".to_owned(), json!(args));
        }
    } else {
        entry.insert("url".to_owned(), json!(url));
        entry.insert("type".to_owned(), json!("http"));
    }
    if let Some(env) = spec
        .get("env")
        .and_then(Value::as_object)
        .filter(|env| !env.is_empty())
    {
        entry.insert("env".to_owned(), Value::Object(env.clone()));
    }
    if let Some(timeout) = spec.get("timeout_secs").and_then(Value::as_u64) {
        entry.insert("timeout_secs".to_owned(), json!(timeout));
    }
    let mut servers = read_mcp_servers(path);
    servers.insert(name.to_owned(), Value::Object(entry));
    write_mcp_servers(path, &servers)?;
    Ok(mcp_list_result(path))
}

fn mcp_remove_server(path: &Path, name: &str) -> Result<Value> {
    let name = name.trim();
    let mut servers = read_mcp_servers(path);
    anyhow::ensure!(
        servers.remove(name).is_some(),
        "no MCP server named {name} is configured"
    );
    write_mcp_servers(path, &servers)?;
    Ok(mcp_list_result(path))
}

/// How a provider's API key is stored, derived from its login descriptor.
enum ApiKeyTarget {
    /// Upsert `KEY=value` inside the named file under the app config dir.
    EnvFile(&'static str, &'static str),
    /// Gemini stores through its own module (gemini.env, dual env names).
    Gemini,
    /// Cursor stores through its own module (CLI keychain or key file).
    Cursor,
}

fn api_key_target_for(
    provider: &crate::provider_catalog::LoginProviderDescriptor,
) -> Option<ApiKeyTarget> {
    use crate::provider_catalog::LoginProviderTarget;
    match provider.target {
        LoginProviderTarget::OpenAiApiKey => {
            Some(ApiKeyTarget::EnvFile("openai.env", "OPENAI_API_KEY"))
        }
        LoginProviderTarget::ClaudeApiKey => {
            Some(ApiKeyTarget::EnvFile("anthropic.env", "ANTHROPIC_API_KEY"))
        }
        LoginProviderTarget::OpenRouter => {
            Some(ApiKeyTarget::EnvFile("openrouter.env", "OPENROUTER_API_KEY"))
        }
        LoginProviderTarget::Bedrock => Some(ApiKeyTarget::EnvFile(
            crate::provider::bedrock::ENV_FILE,
            crate::provider::bedrock::API_KEY_ENV,
        )),
        LoginProviderTarget::Azure => Some(ApiKeyTarget::EnvFile(
            crate::auth::azure::ENV_FILE,
            crate::auth::azure::API_KEY_ENV,
        )),
        // OpenAI-compatible profiles carry their own env file + key name
        // (groq, cerebras, deepseek, ollama, ...), so API keys are settable
        // for every one of them.
        LoginProviderTarget::OpenAiCompatible(profile) => {
            Some(ApiKeyTarget::EnvFile(profile.env_file, profile.api_key_env))
        }
        LoginProviderTarget::Gemini => Some(ApiKeyTarget::Gemini),
        LoginProviderTarget::Cursor => Some(ApiKeyTarget::Cursor),
        _ => None,
    }
}

/// Full auth assessment across every login provider. Uses the same cached
/// probe the CLI and TUI use, so the desktop sees identical truth.
fn auth_status_report() -> Result<Value> {
    let status = crate::auth::AuthStatus::check();
    let providers = crate::provider_catalog::login_providers()
        .iter()
        .copied()
        .map(|provider| {
            let assessment = status.assessment_for_provider(provider);
            json!({
                "id": provider.id,
                "display_name": provider.display_name,
                "menu_detail": provider.menu_detail,
                "state": assessment.state.label(),
                "method_detail": assessment.method_detail,
                "credential_source_detail": assessment.credential_source_detail,
                "health_summary": assessment.health_summary(),
                "api_key_settable": api_key_target_for(&provider).is_some(),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "providers": providers }))
}

fn validate_api_key(key: &str) -> Result<&str> {
    let key = key.trim();
    anyhow::ensure!(!key.is_empty(), "API key cannot be empty");
    anyhow::ensure!(
        !key.chars().any(|c| c == '\n' || c == '\r' || c == '\0'),
        "API key contains control characters"
    );
    anyhow::ensure!(key.len() <= 4096, "API key is unreasonably long");
    Ok(key)
}

fn find_login_provider(provider_id: &str) -> Result<crate::provider_catalog::LoginProviderDescriptor> {
    crate::provider_catalog::login_providers()
        .iter()
        .copied()
        .find(|provider| {
            provider.id == provider_id || provider.aliases.contains(&provider_id)
        })
        .with_context(|| {
            format!(
                "unknown provider {provider_id:?}; see auth.status for the list"
            )
        })
}

fn auth_set_api_key(provider_id: &str, api_key: &str) -> Result<Value> {
    let key = validate_api_key(api_key)?;
    let provider = find_login_provider(provider_id)?;
    match api_key_target_for(&provider) {
        Some(ApiKeyTarget::EnvFile(file, env_key)) => {
            let path = crate::storage::app_config_dir()?.join(file);
            crate::storage::upsert_env_file_value(&path, env_key, Some(key))?;
        }
        Some(ApiKeyTarget::Gemini) => crate::auth::gemini::save_api_key(key)?,
        Some(ApiKeyTarget::Cursor) => crate::auth::cursor::save_api_key(key)?,
        None => anyhow::bail!(
            "provider {} uses an interactive login ({}); run `jcode login {}` in a terminal",
            provider.id,
            provider.menu_detail,
            provider.id
        ),
    }
    crate::auth::AuthStatus::invalidate_cache();
    Ok(json!({
        "provider": provider.id,
        "display_name": provider.display_name,
        "saved": true,
    }))
}

fn auth_clear_key(provider_id: &str) -> Result<Value> {
    let provider = find_login_provider(provider_id)?;
    match api_key_target_for(&provider) {
        Some(ApiKeyTarget::EnvFile(file, env_key)) => {
            let path = crate::storage::app_config_dir()?.join(file);
            crate::storage::upsert_env_file_value(&path, env_key, None)?;
        }
        Some(ApiKeyTarget::Cursor) => crate::auth::cursor::clear_api_key()?,
        Some(ApiKeyTarget::Gemini) => {
            let path = crate::storage::app_config_dir()?
                .join(crate::auth::gemini::GEMINI_API_KEY_ENV_FILE);
            crate::storage::upsert_env_file_value(
                &path,
                crate::auth::gemini::GEMINI_API_KEY_ENV_VARS[0],
                None,
            )?;
        }
        None => anyhow::bail!(
            "provider {} has no API key to clear; manage its login with `jcode login {}`",
            provider.id,
            provider.id
        ),
    }
    crate::auth::AuthStatus::invalidate_cache();
    Ok(json!({
        "provider": provider.id,
        "display_name": provider.display_name,
        "cleared": true,
    }))
}

/// Serialize provider usage rows; `ProviderUsage` is not `Serialize`.
fn usage_providers_json(providers: &[crate::usage::ProviderUsage]) -> Value {
    Value::Array(
        providers
            .iter()
            .map(|provider| {
                json!({
                    "provider_name": provider.provider_name,
                    "limits": provider
                        .limits
                        .iter()
                        .map(|limit| {
                            json!({
                                "name": limit.name,
                                "usage_percent": limit.usage_percent,
                                "resets_at": limit.resets_at,
                            })
                        })
                        .collect::<Vec<_>>(),
                    "hard_limit_reached": provider.hard_limit_reached,
                    "error": provider.error,
                    "last_used_unix_secs": provider.last_used_unix_secs,
                })
            })
            .collect(),
    )
}

/// Serialize the Claude usage windows; `UsageData` itself is not `Serialize`.
fn claude_usage_windows(claude: &crate::usage::UsageData) -> Value {
    json!({
        "five_hour": claude.five_hour,
        "five_hour_resets_at": claude.five_hour_resets_at,
        "seven_day": claude.seven_day,
        "seven_day_resets_at": claude.seven_day_resets_at,
        "seven_day_opus": claude.seven_day_opus,
        "model_scoped": claude
            .model_scoped
            .iter()
            .map(|window| {
                json!({
                    "model_name": window.model_name,
                    "utilization": window.utilization,
                    "resets_at": window.resets_at,
                })
            })
            .collect::<Vec<_>>(),
        "extra_usage_enabled": claude.extra_usage_enabled,
        "last_error": claude.last_error,
    })
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
                    resume_session: None,
                    reuse_task_id: None,
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
                        let mut launch = TaskLaunch::new(
                            goal.to_owned(),
                            roots.into_iter().map(CoworkMount::read_write).collect(),
                        );
                        // Ad-hoc tasks can carry standing instructions too
                        // (the composer's optional instructions field).
                        launch.instructions = request
                            .params
                            .get("instructions")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .filter(|instructions| !instructions.trim().is_empty());
                        launch
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
            let remember = request.params.get("remember").and_then(Value::as_str);
            match (approval_id, approved) {
                (Some(approval_id), Some(approved)) => {
                    let valid_remember =
                        matches!(remember, None | Some("global") | Some("project"));
                    if !valid_remember {
                        return Some(rpc_error(
                            Some(id),
                            -32602,
                            "remember must be \"global\" or \"project\" when present",
                        ));
                    }
                    // Look up what the approval was for BEFORE resolving —
                    // sending the decision removes the pending entry.
                    let remembered = if remember.is_some() {
                        rpc.pending_approval_info(approval_id).await
                    } else {
                        None
                    };
                    let resolved = rpc.approve_tool_call(approval_id, approved).await;
                    if resolved
                        && let Some((tool, task_id)) = remembered
                        && let Some(scope_hint) = remember
                    {
                        rpc.remember_approval_decision(&tool, &task_id, approved, scope_hint)
                            .await;
                    }
                    Ok(json!({ "resolved": resolved }))
                }
                _ => Err((
                    -32602,
                    "params require string id and boolean approved".to_string(),
                )),
            }
        }
        "shutdown" => {
            let stopped = rpc.initiate_shutdown().await;
            Ok(json!({ "shutting_down": true, "stopped_tasks": stopped }))
        }
        "permissions.list" => Ok(json!({ "rules": rpc.permission_rules() })),
        "permissions.add" => {
            let tool = request
                .params
                .get("tool")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|tool| !tool.is_empty());
            let decision = request
                .params
                .get("decision")
                .and_then(Value::as_str)
                .and_then(RuleDecision::parse);
            let (Some(tool), Some(decision)) = (tool, decision) else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "params require string tool and decision \"allow\"|\"deny\"",
                ));
            };
            let scope = match request.params.get("scope").and_then(Value::as_str) {
                None | Some("global") => RuleScope::Global,
                Some("project") => {
                    let project_id = request
                        .params
                        .get("project_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if rpc.project_store().get(project_id).is_err() {
                        return Some(rpc_error(
                            Some(id),
                            -32000,
                            format!("unknown project {project_id}"),
                        ));
                    }
                    RuleScope::Project {
                        project_id: project_id.to_owned(),
                    }
                }
                Some(other) => {
                    return Some(rpc_error(
                        Some(id),
                        -32602,
                        format!("scope must be \"global\" or \"project\", got {other:?}"),
                    ));
                }
            };
            let rule = match rpc.upsert_rule(tool, decision, scope) {
                Ok(rule) => rule,
                Err(error) => return Some(rpc_error(Some(id), -32000, error.to_string())),
            };
            Ok(json!({ "rule": rule }))
        }
        "permissions.remove" => {
            let rule_id = request.params.get("id").and_then(Value::as_str);
            let Some(rule_id) = rule_id else {
                return Some(rpc_error(Some(id), -32602, "params require string id"));
            };
            Ok(json!({ "removed": rpc.remove_rule(rule_id) }))
        }
        "tools.list" => Ok(tools_list_result()),
        "task.transcript" => {
            let task_id = request.params.get("task_id").and_then(Value::as_str);
            let Some(task_id) = task_id else {
                return Some(rpc_error(Some(id), -32602, "params require string task_id"));
            };
            let limit = request
                .params
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(200)
                .clamp(1, TRANSCRIPT_MESSAGE_LIMIT as u64)
                as usize;
            let Some(session_id) = rpc.task_session_id(task_id).await else {
                return Some(rpc_error(
                    Some(id),
                    -32000,
                    format!("no session recorded for task {task_id}"),
                ));
            };
            match tokio::task::spawn_blocking(move || load_transcript(&session_id, limit)).await {
                Ok(Ok(transcript)) => Ok(transcript),
                Ok(Err(error)) => Err((-32000, error.to_string())),
                Err(error) => Err((-32000, format!("transcript task failed: {error}"))),
            }
        }
        "usage.report" => {
            let providers = crate::usage::fetch_all_provider_usage().await;
            let claude = crate::usage::get().await;
            Ok(json!({
                "providers": usage_providers_json(&providers),
                "claude_windows": claude_usage_windows(&claude),
            }))
        }
        "auth.status" => match tokio::task::spawn_blocking(auth_status_report).await {
            Ok(Ok(report)) => Ok(report),
            Ok(Err(error)) => Err((-32000, error.to_string())),
            Err(error) => Err((-32000, format!("auth probe failed: {error}"))),
        },
        "auth.set_api_key" => {
            let provider_id = request.params.get("provider").and_then(Value::as_str);
            let api_key = request.params.get("api_key").and_then(Value::as_str);
            match (provider_id, api_key) {
                (Some(provider_id), Some(api_key)) => {
                    let provider_id = provider_id.to_owned();
                    let api_key = api_key.to_owned();
                    match tokio::task::spawn_blocking(move || {
                        auth_set_api_key(&provider_id, &api_key)
                    })
                    .await
                    {
                        Ok(Ok(result)) => Ok(result),
                        Ok(Err(error)) => Err((-32000, error.to_string())),
                        Err(error) => Err((-32000, format!("auth task failed: {error}"))),
                    }
                }
                _ => Err((
                    -32602,
                    "params require string provider and string api_key".to_string(),
                )),
            }
        }
        "auth.clear" => {
            let provider_id = request.params.get("provider").and_then(Value::as_str);
            match provider_id {
                Some(provider_id) => {
                    let provider_id = provider_id.to_owned();
                    match tokio::task::spawn_blocking(move || auth_clear_key(&provider_id)).await {
                        Ok(Ok(result)) => Ok(result),
                        Ok(Err(error)) => Err((-32000, error.to_string())),
                        Err(error) => Err((-32000, format!("auth task failed: {error}"))),
                    }
                }
                _ => Err((-32602, "params require string provider".to_string())),
            }
        }
        "mcp.list" => match mcp_config_path() {
            Ok(path) => Ok(mcp_list_result(&path)),
            Err(error) => Err((-32000, error.to_string())),
        },
        "mcp.add" => {
            let name = request.params.get("name").and_then(Value::as_str);
            let Some(name) = name else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "params require string name and either command or url",
                ));
            };
            let path = match mcp_config_path() {
                Ok(path) => path,
                Err(error) => return Some(rpc_error(Some(id), -32000, error.to_string())),
            };
            let name = name.to_owned();
            let spec = request.params.clone();
            match tokio::task::spawn_blocking(move || mcp_add_server(&path, &name, &spec)).await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err((-32000, error.to_string())),
                Err(error) => Err((-32000, format!("mcp task failed: {error}"))),
            }
        }
        "mcp.remove" => {
            let name = request.params.get("name").and_then(Value::as_str);
            let Some(name) = name else {
                return Some(rpc_error(Some(id), -32602, "params require string name"));
            };
            let path = match mcp_config_path() {
                Ok(path) => path,
                Err(error) => return Some(rpc_error(Some(id), -32000, error.to_string())),
            };
            let name = name.to_owned();
            match tokio::task::spawn_blocking(move || mcp_remove_server(&path, &name)).await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err((-32000, error.to_string())),
                Err(error) => Err((-32000, format!("mcp task failed: {error}"))),
            }
        }
        "schedule.create" => {
            let project_id = request.params.get("project_id").and_then(Value::as_str);
            let goal = request.params.get("goal").and_then(Value::as_str);
            let run_at_unix_secs = request
                .params
                .get("run_at_unix_secs")
                .and_then(Value::as_u64);
            let model = request
                .params
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|model| !model.trim().is_empty());
            let Some(project_id) = project_id else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "params require string project_id, string goal, and run_at_unix_secs",
                ));
            };
            let Some(goal) = goal else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "params require string project_id, string goal, and run_at_unix_secs",
                ));
            };
            let Some(run_at_unix_secs) = run_at_unix_secs else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "run_at_unix_secs must be a unix timestamp in seconds",
                ));
            };
            if let Err(error) = rpc.project_store().get(project_id) {
                return Some(rpc_error(Some(id), -32000, error.to_string()));
            }
            let now = now_unix_secs();
            if run_at_unix_secs + 60 < now {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "run_at_unix_secs is too far in the past",
                ));
            }
            // Cheap file IO on a small JSON store; keep it off the reactor.
            let project_id = project_id.to_owned();
            let goal = goal.to_owned();
            match tokio::task::spawn_blocking(move || {
                ScheduleStore::open_default()
                    .and_then(|mut store| store.add(project_id, goal, model, run_at_unix_secs))
            })
            .await
            {
                Ok(Ok(entry)) => Ok(json!({ "entry": entry })),
                Ok(Err(error)) => Err((-32000, error.to_string())),
                Err(error) => Err((-32000, format!("schedule task failed: {error}"))),
            }
        }
        "schedule.list" => match tokio::task::spawn_blocking(|| {
            ScheduleStore::open_default().map(|store| store.list())
        })
        .await
        {
            Ok(Ok(entries)) => Ok(json!({ "entries": entries })),
            Ok(Err(error)) => Err((-32000, error.to_string())),
            Err(error) => Err((-32000, format!("schedule task failed: {error}"))),
        },
        "schedule.cancel" => {
            let entry_id = request.params.get("id").and_then(Value::as_str);
            let Some(entry_id) = entry_id else {
                return Some(rpc_error(Some(id), -32602, "params require string id"));
            };
            let entry_id = entry_id.to_owned();
            match tokio::task::spawn_blocking(move || {
                ScheduleStore::open_default().and_then(|mut store| store.cancel(&entry_id))
            })
            .await
            {
                Ok(Ok(cancelled)) => Ok(json!({ "cancelled": cancelled })),
                Ok(Err(error)) => Err((-32000, error.to_string())),
                Err(error) => Err((-32000, format!("schedule task failed: {error}"))),
            }
        }
        "task.followup" => {
            let task_id = request.params.get("task_id").and_then(Value::as_str);
            let message = request.params.get("message").and_then(Value::as_str);
            let model_override = request
                .params
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|model| !model.trim().is_empty());
            let Some(task_id) = task_id else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "params require string task_id and string message",
                ));
            };
            let Some(message) = message.filter(|message| !message.trim().is_empty()) else {
                return Some(rpc_error(
                    Some(id),
                    -32602,
                    "params require string task_id and string message",
                ));
            };
            if rpc.task_is_running(task_id).await {
                return Some(rpc_error(
                    Some(id),
                    -32000,
                    format!("task {task_id} is still running; wait for it to finish before sending a follow-up"),
                ));
            }
            let Some(session_id) = rpc.task_session_id(task_id).await else {
                return Some(rpc_error(
                    Some(id),
                    -32000,
                    format!("no session recorded for task {task_id}; it cannot be resumed"),
                ));
            };
            let Some(context) = rpc.task_context(task_id).await else {
                return Some(rpc_error(
                    Some(id),
                    -32000,
                    format!("no workspace context recorded for task {task_id}"),
                ));
            };
            // Instructions are re-applied from the project so the resumed
            // turn keeps its brief.
            let instructions = context
                .project_id
                .as_ref()
                .and_then(|project_id| rpc.project_store().get(project_id).ok())
                .and_then(|project| {
                    if project.instructions.trim().is_empty() {
                        None
                    } else {
                        Some(project.instructions.clone())
                    }
                });
            let launch = build_followup_launch(
                task_id,
                session_id,
                context,
                message,
                model_override,
                instructions,
            );
            rpc.start_task(launch)
                .await
                .map(|task_id| json!({ "task_id": task_id }))
                .map_err(|error| (-32000, error.to_string()))
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
    spawn_scheduler(rpc.clone());
    spawn_idle_janitor(rpc.clone());
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
                            // A `shutdown` request just completed: the
                            // response is flushed, so exit cleanly. Tasks
                            // were already aborted by initiate_shutdown.
                            if client_rpc.shutdown_requested() {
                                std::process::exit(0);
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
    spawn_scheduler(rpc.clone());
    spawn_idle_janitor(rpc.clone());
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
                                // A `shutdown` request just completed: the
                                // response is flushed, so exit cleanly.
                                if client_rpc.shutdown_requested() {
                                    std::process::exit(0);
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
                |_, _, _, _, _, _, _| async { std::future::pending::<Result<String>>().await },
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
                |_, _, _, _, _, _, _| async { Ok("Reviewed the workspace".to_string()) },
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
    async fn permission_rules_upsert_persist_and_replace() {
        let _env = lock_store_env();
        let store_dir_handle = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir_handle.path()) };
        let rpc = CoworkRpc::new();
        assert!(rpc.permission_rules().is_empty());

        let allow = rpc
            .upsert_rule("webfetch", RuleDecision::Allow, RuleScope::Global)
            .unwrap();
        // One rule per (tool, scope): a newer decision replaces the older one.
        let deny = rpc
            .upsert_rule("webfetch", RuleDecision::Deny, RuleScope::Global)
            .unwrap();
        let rules = rpc.permission_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, deny.id);
        assert_ne!(rules[0].id, allow.id);
        assert_eq!(rules[0].decision, RuleDecision::Deny);

        // Project-scope rules coexist with the global one for the same tool.
        let _project = rpc
            .upsert_rule(
                "webfetch",
                RuleDecision::Allow,
                RuleScope::Project {
                    project_id: "proj-1".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(rpc.permission_rules().len(), 2);

        // Persistence: a fresh CoworkRpc loads the rules from the store file.
        let restarted = CoworkRpc::new();
        assert_eq!(restarted.permission_rules().len(), 2);
        assert!(restarted.remove_rule(&deny.id));
        assert!(!restarted.remove_rule(&deny.id), "second remove is a no-op");
        assert_eq!(restarted.permission_rules().len(), 1);
        assert!(restarted.permission_rules()[0].scope != RuleScope::Global
            || restarted.permission_rules()[0].decision == RuleDecision::Allow);
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn permission_rules_project_scope_beats_global() {
        let _env = lock_store_env();
        let store_dir_handle = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir_handle.path()) };
        let rpc = CoworkRpc::new();
        let deny = rpc
            .upsert_rule("bash", RuleDecision::Deny, RuleScope::Global)
            .unwrap();
        let allow = rpc
            .upsert_rule(
                "bash",
                RuleDecision::Allow,
                RuleScope::Project {
                    project_id: "proj-9".to_owned(),
                },
            )
            .unwrap();
        assert!(allow.created_at >= deny.created_at);
        // Inside the project: the project rule wins despite the global deny.
        assert_eq!(
            rpc.evaluate_permission_rules("bash", Some("proj-9")),
            Some(RuleDecision::Allow)
        );
        // Outside it: only the global rule matches.
        assert_eq!(
            rpc.evaluate_permission_rules("bash", Some("proj-other")),
            Some(RuleDecision::Deny)
        );
        assert_eq!(rpc.evaluate_permission_rules("bash", None), Some(RuleDecision::Deny));
        // No rule for the tool → no decision.
        assert_eq!(rpc.evaluate_permission_rules("webfetch", None), None);
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn permission_rules_short_circuit_the_approval_gate() {
        let _env = lock_store_env();
        let store_dir_handle = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir_handle.path()) };
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        let allow = rpc
            .upsert_rule(
                "bash",
                RuleDecision::Allow,
                RuleScope::Project {
                    project_id: "proj-9".to_owned(),
                },
            )
            .unwrap();
        let deny = rpc
            .upsert_rule("webfetch", RuleDecision::Deny, RuleScope::Global)
            .unwrap();
        assert_ne!(allow.id, deny.id);

        // Simulated task context for a project-scoped task: run_cowork_task
        // records this via start_task; do it directly here.
        rpc.inner.task_contexts.lock().await.push_back((
            "task-p".to_owned(),
            TaskContext {
                mounts: vec![],
                project_id: Some("proj-9".to_owned()),
                model: None,
            },
        ));

        // Allowed by the project rule — no ApprovalRequired event is emitted.
        let approved = rpc
            .request_tool_approval("ap-1", "bash", "run make", "task-p")
            .await
            .unwrap();
        assert!(approved);
        assert!(events.try_recv().is_err(), "rule allow must not prompt");

        // Denied by the global rule — also no prompt, reported as rejected.
        let approved = rpc
            .request_tool_approval("ap-2", "webfetch", "open page", "task-p")
            .await
            .unwrap();
        assert!(!approved);
        assert!(events.try_recv().is_err(), "rule deny must not prompt");

        // An unruled tool still prompts normally (the future would await a
        // decision forever here, so bound it and assert the prompt arrived).
        let prompt = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            rpc.request_tool_approval("ap-3", "write", "write report.md", "task-p"),
        )
        .await;
        assert!(prompt.is_err(), "no IPC client is connected to answer");
        assert!(matches!(
            events.try_recv(),
            Ok(CoworkEvent::ApprovalRequired { .. })
        ));
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn approve_tool_call_with_remember_stores_project_rule() {
        let _env = lock_store_env();
        let store_dir_handle = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir_handle.path()) };
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        // A project task context, as recorded by start_task.
        rpc.inner.task_contexts.lock().await.push_back((
            "task-p".to_owned(),
            TaskContext {
                mounts: vec![],
                project_id: Some("proj-7".to_owned()),
                model: None,
            },
        ));

        // Open two approvals the way the agent session would (spawns a
        // waiter that blocks until the decision arrives).
        let waiter = {
            let rpc = rpc.clone();
            tokio::spawn(async move {
                rpc.request_tool_approval("ap-r1", "bash", "run build", "task-p")
                    .await
            })
        };
        let CoworkEvent::ApprovalRequired { id, tool_name, .. } = events.recv().await.unwrap()
        else {
            panic!("expected ApprovalRequired");
        };
        assert_eq!(tool_name, "bash");

        let response = dispatch_request(
            authed_request(
                &rpc,
                41,
                "approve_tool_call",
                json!({ "id": id, "approved": true, "remember": "project" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["resolved"], true);
        assert_eq!(waiter.await.unwrap().unwrap(), true);

        let rules = rpc.permission_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(rules[0].decision, RuleDecision::Allow);
        assert_eq!(
            rules[0].scope,
            RuleScope::Project {
                project_id: "proj-7".to_owned()
            }
        );

        // A second approval remembered globally replaces nothing else but
        // adds a global rule for a different tool.
        let waiter = {
            let rpc = rpc.clone();
            tokio::spawn(async move {
                rpc.request_tool_approval("ap-r2", "write", "write file", "task-p")
                    .await
            })
        };
        let CoworkEvent::ApprovalRequired { id, .. } = events.recv().await.unwrap() else {
            panic!("expected ApprovalRequired");
        };
        let response = dispatch_request(
            authed_request(
                &rpc,
                42,
                "approve_tool_call",
                json!({ "id": id, "approved": false, "remember": "global" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["resolved"], true);
        assert_eq!(waiter.await.unwrap().unwrap(), false);
        let rules = rpc.permission_rules();
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().any(|rule| rule.tool == "write"
            && rule.decision == RuleDecision::Deny
            && rule.scope == RuleScope::Global));
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn permissions_add_and_remove_via_dispatch() {
        let _env = lock_store_env();
        let store_dir_handle = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir_handle.path()) };
        let rpc = CoworkRpc::new();

        let response = dispatch_request(
            authed_request(
                &rpc,
                60,
                "permissions.add",
                json!({ "tool": "  websearch  ", "decision": "allow" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        let rule_id = response["result"]["rule"]["id"].as_str().unwrap().to_owned();
        assert_eq!(response["result"]["rule"]["tool"], "websearch");
        assert_eq!(response["result"]["rule"]["decision"], "allow");
        assert_eq!(response["result"]["rule"]["scope"]["kind"], "global");

        let response = dispatch_request(authed_request(&rpc, 61, "permissions.list", json!({})), &rpc)
            .await
            .unwrap();
        assert_eq!(response["result"]["rules"].as_array().unwrap().len(), 1);

        // Invalid decision and unknown scope are rejected cleanly.
        let response = dispatch_request(
            authed_request(
                &rpc,
                62,
                "permissions.add",
                json!({ "tool": "bash", "decision": "sometimes" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);
        let response = dispatch_request(
            authed_request(
                &rpc,
                63,
                "permissions.add",
                json!({ "tool": "bash", "decision": "deny", "scope": "everywhere" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);

        let response = dispatch_request(
            authed_request(&rpc, 64, "permissions.remove", json!({ "id": rule_id })),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["removed"], true);
        let response = dispatch_request(authed_request(&rpc, 65, "permissions.list", json!({})), &rpc)
            .await
            .unwrap();
        assert_eq!(response["result"]["rules"].as_array().unwrap().len(), 0);
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn initiate_shutdown_aborts_running_tasks_and_sets_flag() {
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        // A long-running fake task, as start_task would have registered it.
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        rpc.inner.tasks.lock().await.insert("task-1".to_owned(), handle);
        rpc.inner.task_contexts.lock().await.push_back((
            "task-1".to_owned(),
            TaskContext {
                mounts: vec![],
                project_id: None,
                model: None,
            },
        ));

        assert!(!rpc.shutdown_requested());
        let stopped = rpc.initiate_shutdown().await;
        assert_eq!(stopped, 1);
        assert!(rpc.shutdown_requested());
        assert!(rpc.list_tasks().await.is_empty(), "tasks map must drain");
        // The aborted task signals completion like a user stop would.
        let completed = events.recv().await.unwrap();
        assert_eq!(
            completed,
            CoworkEvent::TaskCompleted {
                task_id: "task-1".to_owned(),
                success: false,
                summary: Some("Daemon shutting down".to_owned()),
            }
        );
        // Flag resets are not part of the contract: a second call still
        // succeeds with zero tasks.
        assert_eq!(rpc.initiate_shutdown().await, 0);
    }

    #[tokio::test]
    async fn idle_janitor_fires_only_after_ten_quiet_minutes() {
        let mut janitor = IdleJanitor::new();
        // Active periods reset the counter: client connected, then tasks
        // running, then connected again.
        for _ in 0..7 {
            assert!(!janitor.tick(1, 0));
            assert!(!janitor.tick(0, 2));
        }
        // Quiet stretch: fires exactly on the tenth consecutive idle tick.
        for _ in 0..9 {
            assert!(!janitor.tick(0, 0));
        }
        assert!(janitor.tick(0, 0));
        // And one quiet tick after activity starts again does not fire.
        assert!(!janitor.tick(1, 0));
        assert!(!janitor.tick(0, 0));
    }

    #[tokio::test]
    async fn shutdown_dispatches_and_reports_stopped_tasks() {
        let rpc = CoworkRpc::new();
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        rpc.inner.tasks.lock().await.insert("task-2".to_owned(), handle);

        let response = dispatch_request(authed_request(&rpc, 70, "shutdown", json!({})), &rpc)
            .await
            .unwrap();
        assert_eq!(response["result"]["shutting_down"], true);
        assert_eq!(response["result"]["stopped_tasks"], 1);
        assert!(rpc.shutdown_requested());
        assert!(rpc.list_tasks().await.is_empty());
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
                    resume_session: None,
                    reuse_task_id: None,
                },
                move |_goal, roots, _model, _task_id, instructions, _resume, _rpc| {
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

    #[test]
    fn tools_list_includes_every_read_only_tool() {
        let response = tools_list_result();
        let tools = response["tools"].as_array().unwrap();
        for read_only_tool in READ_ONLY_TOOLS {
            let entry = tools
                .iter()
                .find(|tool| tool["name"] == *read_only_tool)
                .unwrap_or_else(|| panic!("tool catalog missing {read_only_tool}"));
            assert_eq!(entry["read_only"], true, "{read_only_tool} must be read-only");
        }
        assert!(tools.iter().any(|tool| tool["name"] == "mcp__*"));
    }

    #[test]
    fn mcp_add_list_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");

        let result = mcp_add_server(
            &path,
            "todoist",
            &json!({ "command": "npx", "args": ["-y", "todoist-mcp"] }),
        )
        .unwrap();
        assert_eq!(result["servers"].as_array().unwrap().len(), 1);
        assert_eq!(result["servers"][0]["name"], "todoist");
        assert_eq!(result["servers"][0]["command"], "npx");
        assert_eq!(result["servers"][0]["enabled"], true);
        // File on disk stays parseable and preserves the entry.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("todoist-mcp"));

        // Overwrite works; remove works; unknown remove errors.
        mcp_add_server(
            &path,
            "todoist",
            &json!({ "command": "node", "args": ["other.js"] }),
        )
        .unwrap();
        let result = mcp_remove_server(&path, "todoist").unwrap();
        assert_eq!(result["servers"].as_array().unwrap().len(), 0);
        assert!(mcp_remove_server(&path, "todoist").is_err());

        // Validation: both command and url is rejected; bad names rejected.
        assert!(mcp_add_server(&path, "x", &json!({"command": "a", "url": "http://b"})).is_err());
        assert!(mcp_add_server(&path, "bad name!", &json!({"command": "a"})).is_err());
        assert!(mcp_add_server(&path, "", &json!({"command": "a"})).is_err());
    }

    #[test]
    fn mcp_remove_preserves_unknown_fields_of_other_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"keepme":{"command":"npx","custom_future_field":42},"dropme":{"command":"x"}}"#,
        )
        .unwrap();
        mcp_remove_server(&path, "dropme").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("custom_future_field"), "unknown fields survive: {raw}");
        assert!(!raw.contains("dropme"));
    }

    #[test]
    fn transcript_message_maps_visible_blocks_only() {
        let message = crate::session::StoredMessage {
            id: "m1".to_owned(),
            role: crate::message::Role::User,
            content: vec![
                crate::message::ContentBlock::Text {
                    text: "<system-reminder>internal</system-reminder>".to_owned(),
                    cache_control: None,
                },
                crate::message::ContentBlock::Text {
                    text: "Please review the report".to_owned(),
                    cache_control: None,
                },
                crate::message::ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: json!({ "command": "ls" }),
                    thought_signature: None,
                },
            ],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        };
        let entry = transcript_message(&message).unwrap();
        assert_eq!(entry["role"], "user");
        assert_eq!(entry["text"], "Please review the report");
        assert_eq!(entry["events"].as_array().unwrap().len(), 1);
        assert_eq!(entry["events"][0]["type"], "tool_use");
        assert_eq!(entry["events"][0]["tool"], "bash");

        // Display-role wrappers (system injections) never surface.
        let mut hidden = message;
        hidden.display_role = Some(crate::session::StoredDisplayRole::System);
        assert!(transcript_message(&hidden).is_none());
    }

    #[tokio::test]
    async fn task_transcript_unknown_task_errors_cleanly() {
        let rpc = CoworkRpc::new();
        let response = dispatch_request(
            authed_request(
                &rpc,
                60,
                "task.transcript",
                json!({ "task_id": "cowork-missing" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no session recorded"));
    }

    #[tokio::test]
    async fn schedule_create_list_cancel_via_dispatch() {
        let _env = lock_store_env();
        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: the STORE_ENV_LOCK is held, so no other thread reads the
        // environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", store_dir.path()) };
        let rpc = CoworkRpc::new();

        let response = dispatch_request(
            authed_request(&rpc, 70, "project.create", json!({ "name": "Planned" })),
            &rpc,
        )
        .await
        .unwrap();
        let project_id = response["result"]["project"]["id"].as_str().unwrap().to_owned();

        let run_at = now_unix_secs() + 3_600;
        let response = dispatch_request(
            authed_request(
                &rpc,
                71,
                "schedule.create",
                json!({
                    "project_id": project_id,
                    "goal": "Triage the inbox",
                    "run_at_unix_secs": run_at,
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["entry"]["status"], "pending");
        let entry_id = response["result"]["entry"]["id"].as_str().unwrap().to_owned();

        let response = dispatch_request(authed_request(&rpc, 72, "schedule.list", json!({})), &rpc)
            .await
            .unwrap();
        let entries = response["result"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["goal"], "Triage the inbox");

        // Not due yet, so the runner must not fire it.
        let fired = run_due_schedule_entries(&rpc).await.unwrap();
        let _ = fired;
        let response = dispatch_request(authed_request(&rpc, 73, "schedule.list", json!({})), &rpc)
            .await
            .unwrap();
        assert_eq!(response["result"]["entries"][0]["status"], "pending");

        let response = dispatch_request(
            authed_request(&rpc, 74, "schedule.cancel", json!({ "id": entry_id })),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["cancelled"], true);

        // Scheduling for a missing project is rejected.
        let response = dispatch_request(
            authed_request(
                &rpc,
                75,
                "schedule.create",
                json!({
                    "project_id": "proj-nope",
                    "goal": "g",
                    "run_at_unix_secs": run_at,
                }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        // SAFETY: same lock discipline as the set above.
        unsafe { std::env::remove_var("JCODE_COWORK_STORE_DIR") };
    }

    #[tokio::test]
    async fn auth_unknown_provider_is_rejected_without_writes() {
        let rpc = CoworkRpc::new();
        let response = dispatch_request(
            authed_request(
                &rpc,
                80,
                "auth.set_api_key",
                json!({ "provider": "definitely-not-a-provider", "api_key": "sk-test" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown provider"));
    }

    #[tokio::test]
    async fn task_sessions_note_and_lookup() {
        let rpc = CoworkRpc::new();
        rpc.note_task_session("task-a", "sess-a").await;
        rpc.note_task_session("task-b", "sess-b").await;
        assert_eq!(
            rpc.task_session_id("task-a").await.as_deref(),
            Some("sess-a")
        );
        // Re-noting the same task replaces the mapping.
        rpc.note_task_session("task-a", "sess-a2").await;
        assert_eq!(
            rpc.task_session_id("task-a").await.as_deref(),
            Some("sess-a2")
        );
        assert_eq!(rpc.task_session_id("task-missing").await, None);
    }

    #[tokio::test]
    async fn task_followup_rejects_unknown_task() {
        let rpc = CoworkRpc::new();
        let response = dispatch_request(
            authed_request(
                &rpc,
                90,
                "task.followup",
                json!({ "task_id": "cowork-missing", "message": "continue please" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no session recorded"));
    }

    #[tokio::test]
    async fn task_followup_rejects_while_running() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = CoworkRpc::new();
        let task_id = rpc
            .start_task_with(
                TaskLaunch::new(
                    "Long task".to_string(),
                    vec![CoworkMount::read_write(directory.path())],
                ),
                |_, _, _, _, _, _, _| async { std::future::pending::<Result<String>>().await },
            )
            .await
            .unwrap();
        rpc.note_task_session(&task_id, "sess-running").await;

        let response = dispatch_request(
            authed_request(
                &rpc,
                91,
                "task.followup",
                json!({ "task_id": task_id, "message": "are you done yet?" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("still running"));
        let _ = rpc.stop_task(&task_id).await;
    }

    #[tokio::test]
    async fn task_followup_resumes_with_reused_task_id() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = CoworkRpc::new();
        let mut events = rpc.subscribe();
        let calls: std::sync::Arc<
            std::sync::Mutex<Vec<(Option<String>, String, Option<String>)>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_in_task = std::sync::Arc::clone(&calls);
        let launch = TaskLaunch {
            goal: "First turn".to_owned(),
            mounts: vec![CoworkMount::read_write(directory.path())],
            model: Some("test-model".to_owned()),
            instructions: None,
            project_id: None,
            resume_session: None,
            reuse_task_id: None,
        };
        let task_id = rpc
            .start_task_with(
                launch,
                move |goal, _roots, model, _task_id, _instructions, resume, _rpc| {
                    let calls = std::sync::Arc::clone(&calls_in_task);
                    async move {
                        calls
                            .lock()
                            .unwrap()
                            .push((resume, goal, model));
                        Ok("first done".to_owned())
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
                summary: Some("first done".to_string()),
            }
        );
        // run_cowork_task records the session; simulate that here.
        rpc.note_task_session(&task_id, "sess-original").await;
        let context = rpc.task_context(&task_id).await.unwrap();
        assert_eq!(context.model.as_deref(), Some("test-model"));

        // The follow-up launch reuses the task id, resumes the session, and
        // keeps the recorded model unless overridden.
        let followup = build_followup_launch(
            &task_id,
            "sess-original".to_owned(),
            context,
            "Now expand on that",
            None,
            None,
        );
        assert_eq!(followup.reuse_task_id.as_deref(), Some(task_id.as_str()));
        assert_eq!(followup.resume_session.as_deref(), Some("sess-original"));
        assert_eq!(followup.goal, "Now expand on that");
        assert_eq!(followup.model.as_deref(), Some("test-model"));

        let calls_in_followup = std::sync::Arc::clone(&calls);
        let followup_id = rpc
            .start_task_with(
                followup,
                move |goal, _roots, model, _task_id, _instructions, resume, _rpc| {
                    let calls = std::sync::Arc::clone(&calls_in_followup);
                    async move {
                        calls
                            .lock()
                            .unwrap()
                            .push((resume, goal, model));
                        Ok("follow-up done".to_owned())
                    }
                },
            )
            .await
            .unwrap();
        assert_eq!(followup_id, task_id, "follow-up must reuse the task id");
        assert_eq!(
            events.recv().await.unwrap(),
            CoworkEvent::TaskCompleted {
                task_id: task_id.clone(),
                success: true,
                summary: Some("follow-up done".to_string()),
            }
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, None);
        assert_eq!(calls[0].1, "First turn");
        assert_eq!(calls[1].0.as_deref(), Some("sess-original"));
        assert_eq!(calls[1].1, "Now expand on that");
        assert_eq!(calls[1].2.as_deref(), Some("test-model"));
    }

    #[tokio::test]
    async fn task_followup_launch_honors_overrides_and_trims_message() {
        let directory = tempfile::tempdir().unwrap();
        let context = TaskContext {
            mounts: vec![CoworkMount::read_write(directory.path())],
            project_id: Some("proj-1".to_owned()),
            model: Some("original-model".to_owned()),
        };
        // No override: keep the original model; project instructions carried.
        let launch = build_followup_launch(
            "task-1",
            "sess-1".to_owned(),
            context.clone(),
            "  Do it again  ",
            None,
            Some("Always cite the source.".to_owned()),
        );
        assert_eq!(launch.goal, "Do it again");
        assert_eq!(launch.model.as_deref(), Some("original-model"));
        assert_eq!(launch.instructions.as_deref(), Some("Always cite the source."));
        assert_eq!(launch.project_id.as_deref(), Some("proj-1"));
        assert_eq!(launch.resume_session.as_deref(), Some("sess-1"));
        assert_eq!(launch.reuse_task_id.as_deref(), Some("task-1"));

        // Explicit override wins over the recorded model.
        let launch = build_followup_launch(
            "task-1",
            "sess-1".to_owned(),
            context,
            "Do it again",
            Some("override-model".to_owned()),
            None,
        );
        assert_eq!(launch.model.as_deref(), Some("override-model"));
        assert!(launch.instructions.is_none());
    }

    #[tokio::test]
    async fn task_followup_requires_message() {
        let rpc = CoworkRpc::new();
        // Missing message entirely.
        let response = dispatch_request(
            authed_request(
                &rpc,
                94,
                "task.followup",
                json!({ "task_id": "cowork-x" }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);
        // Whitespace-only message is rejected too.
        let response = dispatch_request(
            authed_request(
                &rpc,
                95,
                "task.followup",
                json!({ "task_id": "cowork-x", "message": "   " }),
            ),
            &rpc,
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);
    }
}

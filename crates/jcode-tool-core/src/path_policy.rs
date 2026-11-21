use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

pub type PathValidator = dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync + 'static;
pub type SessionApprovalRequester = dyn Fn(String, String, String) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send>>
    + Send
    + Sync
    + 'static;

pub type SessionArtifactCreatedNotifier = dyn Fn(ArtifactCreated) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
    + Send
    + Sync
    + 'static;

pub type SessionToolInvokedNotifier = dyn Fn(ToolInvoked) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
    + Send
    + Sync
    + 'static;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCreated {
    pub path: PathBuf,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvoked {
    pub tool_call_id: String,
    pub tool_name: String,
}

struct Registration {
    owner: u64,
    validator: Arc<PathValidator>,
}

type ApprovalRequestersMap = HashMap<String, (u64, Arc<SessionApprovalRequester>)>;
type ArtifactNotifiersMap = HashMap<String, (u64, Arc<SessionArtifactCreatedNotifier>)>;
type ToolInvokedNotifiersMap = HashMap<String, (u64, Arc<SessionToolInvokedNotifier>)>;
type ValidatorsMap = HashMap<String, Registration>;

static VALIDATORS: LazyLock<RwLock<ValidatorsMap>> = LazyLock::new(|| RwLock::new(HashMap::new()));
/// Write-side validators for sessions that distinguish read-only mounts from
/// read/write mounts. When a session registers one, every structured write
/// (patch, edit, office document creation) must resolve inside the writable
/// subset; reads keep consulting the broader read validator above. Sessions
/// without a write validator keep the historical single-validator behavior.
static WRITE_VALIDATORS: LazyLock<RwLock<ValidatorsMap>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static APPROVAL_REQUESTERS: LazyLock<RwLock<ApprovalRequestersMap>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static ARTIFACT_CREATED_NOTIFIERS: LazyLock<RwLock<ArtifactNotifiersMap>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static TOOL_INVOKED_NOTIFIERS: LazyLock<RwLock<ToolInvokedNotifiersMap>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub struct SessionApprovalRegistration {
    session_id: String,
    owner: u64,
}

impl Drop for SessionApprovalRegistration {
    fn drop(&mut self) {
        let mut requesters = APPROVAL_REQUESTERS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if requesters
            .get(&self.session_id)
            .is_some_and(|(owner, _)| *owner == self.owner)
        {
            requesters.remove(&self.session_id);
        }
    }
}

pub fn register_session_approval_requester<F, Fut>(
    session_id: impl Into<String>,
    requester: F,
) -> SessionApprovalRegistration
where
    F: Fn(String, String, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<bool, String>> + Send + 'static,
{
    let session_id = session_id.into();
    let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    let requester: Arc<SessionApprovalRequester> =
        Arc::new(move |id, tool, description| Box::pin(requester(id, tool, description)));
    APPROVAL_REQUESTERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.clone(), (owner, requester));
    SessionApprovalRegistration { session_id, owner }
}

pub async fn request_session_approval(
    session_id: &str,
    id: String,
    tool_name: String,
    description: String,
) -> Result<Option<bool>, String> {
    let requester = APPROVAL_REQUESTERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .map(|(_, requester)| Arc::clone(requester));
    match requester {
        Some(requester) => requester(id, tool_name, description).await.map(Some),
        None => Ok(None),
    }
}

pub struct SessionArtifactCreatedRegistration {
    session_id: String,
    owner: u64,
}

impl Drop for SessionArtifactCreatedRegistration {
    fn drop(&mut self) {
        let mut notifiers = ARTIFACT_CREATED_NOTIFIERS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if notifiers
            .get(&self.session_id)
            .is_some_and(|(owner, _)| *owner == self.owner)
        {
            notifiers.remove(&self.session_id);
        }
    }
}

pub fn register_session_artifact_created_notifier<F, Fut>(
    session_id: impl Into<String>,
    notifier: F,
) -> SessionArtifactCreatedRegistration
where
    F: Fn(ArtifactCreated) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let session_id = session_id.into();
    let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    let notifier: Arc<SessionArtifactCreatedNotifier> =
        Arc::new(move |artifact| Box::pin(notifier(artifact)));
    ARTIFACT_CREATED_NOTIFIERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.clone(), (owner, notifier));
    SessionArtifactCreatedRegistration { session_id, owner }
}

pub async fn notify_session_artifact_created(
    session_id: &str,
    path: PathBuf,
    mime_type: impl Into<String>,
) -> Result<(), String> {
    let notifier = ARTIFACT_CREATED_NOTIFIERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .map(|(_, notifier)| Arc::clone(notifier));
    if let Some(notifier) = notifier {
        notifier(ArtifactCreated {
            path,
            mime_type: mime_type.into(),
        })
        .await?;
    }
    Ok(())
}

pub struct SessionToolInvokedRegistration {
    session_id: String,
    owner: u64,
}

impl Drop for SessionToolInvokedRegistration {
    fn drop(&mut self) {
        let mut notifiers = TOOL_INVOKED_NOTIFIERS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if notifiers
            .get(&self.session_id)
            .is_some_and(|(owner, _)| *owner == self.owner)
        {
            notifiers.remove(&self.session_id);
        }
    }
}

pub fn register_session_tool_invoked_notifier<F, Fut>(
    session_id: impl Into<String>,
    notifier: F,
) -> SessionToolInvokedRegistration
where
    F: Fn(ToolInvoked) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let session_id = session_id.into();
    let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    let notifier: Arc<SessionToolInvokedNotifier> =
        Arc::new(move |invoked| Box::pin(notifier(invoked)));
    TOOL_INVOKED_NOTIFIERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.clone(), (owner, notifier));
    SessionToolInvokedRegistration { session_id, owner }
}

pub async fn notify_session_tool_invoked(
    session_id: &str,
    tool_call_id: impl Into<String>,
    tool_name: impl Into<String>,
) -> Result<(), String> {
    let notifier = TOOL_INVOKED_NOTIFIERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .map(|(_, notifier)| Arc::clone(notifier));
    if let Some(notifier) = notifier {
        notifier(ToolInvoked {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
        })
        .await?;
    }
    Ok(())
}

pub struct SessionPathValidatorRegistration {
    session_id: String,
    owner: u64,
}

impl Drop for SessionPathValidatorRegistration {
    fn drop(&mut self) {
        let mut validators = VALIDATORS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if validators
            .get(&self.session_id)
            .is_some_and(|entry| entry.owner == self.owner)
        {
            validators.remove(&self.session_id);
        }
    }
}

pub fn register_session_path_validator<F>(
    session_id: impl Into<String>,
    validator: F,
) -> SessionPathValidatorRegistration
where
    F: Fn(&Path) -> Result<PathBuf, String> + Send + Sync + 'static,
{
    let session_id = session_id.into();
    let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    VALIDATORS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            session_id.clone(),
            Registration {
                owner,
                validator: Arc::new(validator),
            },
        );
    SessionPathValidatorRegistration { session_id, owner }
}

/// Registration handle for the write-side validator. Dropping it removes the
/// write validator; the read validator (if any) is unaffected.
pub struct SessionWritePathValidatorRegistration {
    session_id: String,
    owner: u64,
}

impl Drop for SessionWritePathValidatorRegistration {
    fn drop(&mut self) {
        let mut validators = WRITE_VALIDATORS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if validators
            .get(&self.session_id)
            .is_some_and(|entry| entry.owner == self.owner)
        {
            validators.remove(&self.session_id);
        }
    }
}

pub fn register_session_write_path_validator<F>(
    session_id: impl Into<String>,
    validator: F,
) -> SessionWritePathValidatorRegistration
where
    F: Fn(&Path) -> Result<PathBuf, String> + Send + Sync + 'static,
{
    let session_id = session_id.into();
    let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    WRITE_VALIDATORS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            session_id.clone(),
            Registration {
                owner,
                validator: Arc::new(validator),
            },
        );
    SessionWritePathValidatorRegistration { session_id, owner }
}

/// Validate a path for a write operation. Falls back to the session's read
/// validator when no write validator is registered so existing sessions
/// (single shared root) are unaffected; when one IS registered, only the
/// writable subset of mounts accepts the path.
pub fn verify_session_write_path(
    session_id: &str,
    path: &Path,
) -> Result<Option<PathBuf>, String> {
    let validator = WRITE_VALIDATORS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .map(|entry| Arc::clone(&entry.validator));
    if let Some(validate) = validator {
        return validate(path).map(Some);
    }
    verify_session_path(session_id, path)
}

pub fn verify_session_path(session_id: &str, path: &Path) -> Result<Option<PathBuf>, String> {
    let validator = VALIDATORS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .map(|entry| Arc::clone(&entry.validator));
    validator.map(|validate| validate(path)).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_validator_checks_resolved_tool_path() {
        let session = format!("path-policy-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed));
        let _registration = register_session_path_validator(&session, |path| {
            if path.starts_with("/workspace") {
                Ok(path.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });

        assert_eq!(
            verify_session_path(&session, Path::new("/workspace/file.txt")).unwrap(),
            Some(PathBuf::from("/workspace/file.txt"))
        );
        assert!(verify_session_path(&session, Path::new("/private/file.txt")).is_err());
    }

    #[test]
    fn dropping_registration_disables_session_validator() {
        let session = format!("path-policy-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed));
        let registration = register_session_path_validator(&session, |path| Ok(path.to_path_buf()));
        assert!(
            verify_session_path(&session, Path::new("/path"))
                .unwrap()
                .is_some()
        );
        drop(registration);
        assert!(
            verify_session_path(&session, Path::new("/path"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_artifact_notifier_receives_created_artifact() {
        let session = format!(
            "artifact-policy-{}",
            NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
        );
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_by_notifier = Arc::clone(&received);
        let _registration = register_session_artifact_created_notifier(&session, move |artifact| {
            let received = Arc::clone(&received_by_notifier);
            async move {
                received.lock().unwrap().push(artifact);
                Ok(())
            }
        });

        notify_session_artifact_created(
            &session,
            PathBuf::from("/workspace/report.xlsx"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )
        .await
        .unwrap();

        assert_eq!(
            *received.lock().unwrap(),
            vec![ArtifactCreated {
                path: PathBuf::from("/workspace/report.xlsx"),
                mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                    .to_owned(),
            }]
        );
    }

    #[test]
    fn write_validator_falls_back_to_read_validator_when_absent() {
        let session = format!("write-fallback-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed));
        let _read = register_session_path_validator(&session, |path| {
            if path.starts_with("/workspace") {
                Ok(path.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });

        assert_eq!(
            verify_session_write_path(&session, Path::new("/workspace/file.txt")).unwrap(),
            Some(PathBuf::from("/workspace/file.txt"))
        );
        assert!(verify_session_write_path(&session, Path::new("/private/x")).is_err());
    }

    #[test]
    fn write_validator_restricts_writes_to_writable_mounts_only() {
        let session = format!("write-split-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed));
        // Read validator accepts both mounts; write validator only /rw.
        let _read = register_session_path_validator(&session, |path| {
            if path.starts_with("/workspace") {
                Ok(path.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });
        let _write = register_session_write_path_validator(&session, |path| {
            if path.starts_with("/workspace/rw") {
                Ok(path.to_path_buf())
            } else {
                Err("SECURITY_VIOLATION: mount is read-only".to_owned())
            }
        });

        // Reads still reach the read-only mount.
        assert!(verify_session_path(&session, Path::new("/workspace/ro/data.txt")).is_ok());
        // Writes to the read-only mount are denied...
        let error = verify_session_write_path(&session, Path::new("/workspace/ro/data.txt"))
            .unwrap_err();
        assert!(error.contains("read-only"), "got: {error}");
        // ...and writes to the read/write mount succeed.
        assert_eq!(
            verify_session_write_path(&session, Path::new("/workspace/rw/summary.xlsx"))
                .unwrap(),
            Some(PathBuf::from("/workspace/rw/summary.xlsx"))
        );
    }

    #[test]
    fn dropping_write_registration_restores_fallback_behavior() {
        let session = format!("write-drop-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed));
        let _read = register_session_path_validator(&session, |path| {
            Ok(path.to_path_buf())
        });
        {
            let _write = register_session_write_path_validator(&session, |_| {
                Err("denied".to_owned())
            });
            assert!(verify_session_write_path(&session, Path::new("/x")).is_err());
        }
        assert_eq!(
            verify_session_write_path(&session, Path::new("/x")).unwrap(),
            Some(PathBuf::from("/x"))
        );
    }
}

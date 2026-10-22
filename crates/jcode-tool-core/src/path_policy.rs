use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

pub type PathValidator = dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync + 'static;
pub type SessionApprovalRequester = dyn Fn(
        String,
        String,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send>>
    + Send
    + Sync
    + 'static;

pub type SessionArtifactCreatedNotifier = dyn Fn(ArtifactCreated)
        -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
    + Send
    + Sync
    + 'static;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCreated {
    pub path: PathBuf,
    pub mime_type: String,
}

struct Registration {
    owner: u64,
    validator: Arc<PathValidator>,
}

static VALIDATORS: LazyLock<RwLock<HashMap<String, Registration>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static APPROVAL_REQUESTERS: LazyLock<RwLock<HashMap<String, (u64, Arc<SessionApprovalRequester>)>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static ARTIFACT_CREATED_NOTIFIERS: LazyLock<
    RwLock<HashMap<String, (u64, Arc<SessionArtifactCreatedNotifier>)>>,
> = LazyLock::new(|| RwLock::new(HashMap::new()));

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
    let requester: Arc<SessionApprovalRequester> = Arc::new(move |id, tool, description| {
        Box::pin(requester(id, tool, description))
    });
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
    let notifier: Arc<SessionArtifactCreatedNotifier> = Arc::new(move |artifact| {
        Box::pin(notifier(artifact))
    });
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
        assert!(verify_session_path(&session, Path::new("/path")).unwrap().is_some());
        drop(registration);
        assert!(verify_session_path(&session, Path::new("/path")).unwrap().is_none());
    }

    #[tokio::test]
    async fn session_artifact_notifier_receives_created_artifact() {
        let session = format!("artifact-policy-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed));
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
}
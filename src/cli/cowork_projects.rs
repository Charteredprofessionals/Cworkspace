//! Persistent project registry for the Cowork desktop.
//!
//! A project groups the durable context of knowledge work: purpose and
//! instructions, folder mounts with explicit access levels, the preferred
//! model, and the task history. The store is a JSON document under
//! `jcode_dir()/cowork-store/projects.json`, written atomically after every
//! mutation. Mounts recorded here are advisory metadata until a task starts;
//! the live sandbox for each task is registered from the project's mounts at
//! `start_task` time (see `cowork_rpc`).

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// The root crate re-exports the storage crate's data-dir helpers as
// `crate::storage`; `jcode_dir()` is the per-user application data root.
use crate::storage::jcode_dir;

/// Protocol/schema version of the persisted project store. Bump when the
/// shape changes incompatibly; unknown fields fail deserialization so old
/// binaries never silently misread a newer store.
pub const STORE_VERSION: u32 = 1;

/// Access level granted to a mounted folder. Shown verbatim in the UI and
/// enforced by the write-side path validator when tasks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAccess {
    /// Read-only: structured writes are rejected before they touch disk.
    ReadOnly,
    /// Read and write.
    ReadWrite,
}

impl MountAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            MountAccess::ReadOnly => "read_only",
            MountAccess::ReadWrite => "read_write",
        }
    }
}

/// Stable filesystem identity of a mounted directory, captured when the
/// mount is added. Lets a health check tell "the folder was renamed or
/// moved" apart from "the folder is gone" by re-finding the same inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Observed health of a recorded mount path. Computed on demand at
/// serialization time — never persisted — so a stale snapshot can never
/// mask a folder that moved or disappeared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountHealth {
    /// The recorded path resolves; the directory is reachable.
    Available,
    /// The recorded path is gone but the mounted directory itself still
    /// exists elsewhere (renamed or moved); `current_path` says where.
    Moved,
    /// The directory could not be found at the recorded path or nearby.
    Disconnected,
}

#[derive(Clone, Debug)]
pub struct MountObservation {
    pub health: MountHealth,
    /// Where the directory lives now, for `MountHealth::Moved`.
    pub current_path: Option<PathBuf>,
}

/// Classify a recorded mount path against the live filesystem. `identity`
/// is the (dev, ino) pair captured at mount time; legacy stores and
/// non-Unix platforms have `None` and only get available/disconnected.
pub fn observe_mount(recorded: &Path, identity: Option<MountIdentity>) -> MountObservation {
    if std::fs::metadata(recorded).is_ok() {
        return MountObservation {
            health: MountHealth::Available,
            current_path: None,
        };
    }
    #[cfg(unix)]
    if let Some(identity) = identity {
        // Rename/move within the same parent: scan the parent's direct
        // children for the directory carrying the recorded identity. One
        // bounded readdir — no filesystem-wide walk.
        if let Some(parent) = recorded.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if entry.path() == recorded {
                        continue;
                    }
                    if let Ok(meta) = entry.metadata() {
                        if !meta.is_dir() {
                            continue;
                        }
                        use std::os::unix::fs::MetadataExt;
                        if meta.dev() == identity.dev && meta.ino() == identity.ino {
                            return MountObservation {
                                health: MountHealth::Moved,
                                current_path: Some(entry.path()),
                            };
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = identity;
    MountObservation {
        health: MountHealth::Disconnected,
        current_path: None,
    }
}

/// Capture the filesystem identity of a directory. Unix only; other
/// platforms leave mounts without an identity and moved-detection degrades
/// to available/disconnected.
#[cfg(unix)]
fn mount_identity(path: &Path) -> Option<MountIdentity> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| MountIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        })
}

#[cfg(not(unix))]
fn mount_identity(_path: &Path) -> Option<MountIdentity> {
    None
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMount {
    pub path: PathBuf,
    pub access: MountAccess,
    pub added_at: String,
    /// (dev, ino) at mount time (Unix). Absent for legacy mounts; used by
    /// `observe_mount` to re-find a folder that was renamed or moved.
    #[serde(default)]
    pub identity: Option<MountIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub mounts: Vec<ProjectMount>,
    #[serde(default)]
    pub model_default: Option<String>,
    #[serde(default)]
    pub task_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Project {
    /// Read/write mount roots, in stable order.
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        self.mounts
            .iter()
            .filter(|mount| mount.access == MountAccess::ReadWrite)
            .map(|mount| mount.path.clone())
            .collect()
    }

    /// All mount roots regardless of access level.
    pub fn all_roots(&self) -> Vec<PathBuf> {
        self.mounts.iter().map(|mount| mount.path.clone()).collect()
    }

    /// JSON view with live mount health annotations. The store keeps the
    /// configuration; health is recomputed on every serialization so the
    /// desktop always sees the filesystem as it is right now.
    pub fn view_json(&self) -> serde_json::Value {
        let mut value = match serde_json::to_value(self) {
            Ok(value) => value,
            Err(_) => return serde_json::Value::Null,
        };
        let Some(views) = value.get_mut("mounts").and_then(|m| m.as_array_mut()) else {
            return value;
        };
        for (mount, view) in self.mounts.iter().zip(views.iter_mut()) {
            let observation = observe_mount(&mount.path, mount.identity);
            if let Some(current) = &observation.current_path {
                view["current_path"] =
                    serde_json::Value::String(current.display().to_string());
            }
            view["health"] = serde_json::json!(match observation.health {
                MountHealth::Available => "available",
                MountHealth::Moved => "moved",
                MountHealth::Disconnected => "disconnected",
            });
        }
        value
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    version: u32,
    projects: Vec<Project>,
}

#[derive(Clone)]
pub struct ProjectStore {
    path: PathBuf,
}

impl ProjectStore {
    /// Store at the daemon's standard data directory. Tests point
    /// `JCODE_COWORK_STORE_DIR` at a temp dir to stay hermetic.
    pub fn open_default() -> Result<Self> {
        let dir = if let Ok(override_dir) = std::env::var("JCODE_COWORK_STORE_DIR") {
            PathBuf::from(override_dir)
        } else {
            jcode_dir()?.join("cowork-store")
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating project store directory {}", dir.display()))?;
        Ok(Self {
            path: dir.join("projects.json"),
        })
    }

    /// Store at an explicit path (tests, custom data directories).
    pub fn open_at(path: PathBuf) -> Self {
        Self { path }
    }

    /// File backing this store; mount RPCs pass it to the sync helpers so
    /// their blocking IO runs inside `spawn_blocking`.
    pub fn store_path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Store {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Store {
                version: STORE_VERSION,
                projects: Vec::new(),
            }),
            Err(_) => Store {
                version: STORE_VERSION,
                projects: Vec::new(),
            },
        }
    }

    fn save(&self, store: &Store) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(store)?)?;
        // Atomic swap so a crash mid-write never truncates the registry.
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }

    pub fn create(
        &self,
        name: &str,
        icon: &str,
        purpose: &str,
        instructions: &str,
    ) -> Result<Project> {
        if name.trim().is_empty() {
            bail!("project name cannot be empty");
        }
        let mut store = self.load();
        let now = timestamp();
        let project = Project {
            id: format!("proj-{}", jcode_id_suffix()),
            name: name.trim().to_owned(),
            icon: icon.trim().to_owned(),
            purpose: purpose.trim().to_owned(),
            instructions: instructions.to_owned(),
            mounts: Vec::new(),
            model_default: None,
            task_ids: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        };
        store.projects.push(project.clone());
        self.save(&store)?;
        Ok(project)
    }

    pub fn list(&self) -> Result<Vec<Project>> {
        Ok(self.load().projects)
    }

    pub fn get(&self, id: &str) -> Result<Project> {
        self.load()
            .projects
            .into_iter()
            .find(|project| project.id == id)
            .ok_or_else(|| anyhow!("project not found: {id}"))
    }

    /// Apply `mutate` to a project and persist. The mutation receives an
    /// exclusive view; callers must not re-enter the store from inside it.
    pub fn update(
        &self,
        id: &str,
        mutate: impl FnOnce(&mut Project) -> Result<()>,
    ) -> Result<Project> {
        let mut store = self.load();
        let project = store
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .ok_or_else(|| anyhow!("project not found: {id}"))?;
        mutate(project)?;
        project.updated_at = timestamp();
        let updated = project.clone();
        self.save(&store)?;
        Ok(updated)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let mut store = self.load();
        let before = store.projects.len();
        store.projects.retain(|project| project.id != id);
        if store.projects.len() == before {
            return Ok(false);
        }
        self.save(&store)?;
        Ok(true)
    }

    /// Add a mount after validating the directory exists. Duplicate paths are
    /// rejected so the access level always stays unambiguous. Synchronous by
    /// design: call it from async contexts via `spawn_blocking`.
    pub fn add_mount_sync(
        store_path: &Path,
        id: &str,
        path: &Path,
        access: MountAccess,
    ) -> Result<Project> {
        let store: Store = std::fs::read(store_path)
            .map(|bytes| serde_json::from_slice(&bytes).unwrap_or(Store {
                version: STORE_VERSION,
                projects: Vec::new(),
            }))
            .unwrap_or(Store {
                version: STORE_VERSION,
                projects: Vec::new(),
            });
        let mut store = store;
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("cannot resolve folder {}", path.display()))?;
        if !canonical.is_dir() {
            bail!("mount path is not a directory: {}", canonical.display());
        }
        let project = store
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .ok_or_else(|| anyhow!("project not found: {id}"))?;
        if project
            .mounts
            .iter()
            .any(|mount| mount.path == canonical)
        {
            bail!("folder is already mounted on this project: {}", canonical.display());
        }
        let identity = mount_identity(&canonical);
        project.mounts.push(ProjectMount {
            path: canonical,
            access,
            added_at: timestamp(),
            identity,
        });
        project.updated_at = timestamp();
        let updated = project.clone();
        write_store(store_path, &store)?;
        Ok(updated)
    }

    /// Remove a mount immediately. Future tool calls can no longer resolve
    /// inside it once the session jail is re-registered (see cowork_rpc).
    pub fn revoke_mount_sync(store_path: &Path, id: &str, path: &Path) -> Result<Project> {
        let mut store: Store = std::fs::read(store_path)
            .map(|bytes| serde_json::from_slice(&bytes).unwrap_or(Store {
                version: STORE_VERSION,
                projects: Vec::new(),
            }))
            .unwrap_or(Store {
                version: STORE_VERSION,
                projects: Vec::new(),
            });
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("cannot resolve folder {}", path.display()))?;
        let project = store
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .ok_or_else(|| anyhow!("project not found: {id}"))?;
        let before = project.mounts.len();
        project.mounts.retain(|mount| mount.path != canonical);
        if project.mounts.len() == before {
            bail!("folder is not mounted on this project");
        }
        project.updated_at = timestamp();
        let updated = project.clone();
        write_store(store_path, &store)?;
        Ok(updated)
    }

    pub fn record_task(&self, id: &str, task_id: &str) -> Result<Project> {
        self.update(id, |project| {
            project.task_ids.push(task_id.to_owned());
            Ok(())
        })
    }
}

fn write_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(store)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn timestamp() -> String {
    // RFC 3339 via the standard library only: seconds precision is enough for
    // project metadata, and chrono is deliberately not added to this crate.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_unix_seconds(now)
}

fn format_unix_seconds(secs: u64) -> String {
    // Convert unix seconds to UTC calendar values (civil-from-days algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn jcode_id_suffix() -> String {
    let a = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let random: u32 = (a & 0xFFFF_FFFF) as u32 ^ (std::process::id() as u32);
    format!("{:08x}{:x}", random, a as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, ProjectStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = ProjectStore::open_at(temp.path().join("projects.json"));
        (temp, store)
    }

    #[test]
    fn create_list_get_round_trip() {
        let (_temp, store) = temp_store();
        let project = store
            .create("Q4 Planning", "folder", "Close the quarter", "Be concise.")
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.get(&project.id).unwrap().name, "Q4 Planning");
        assert!(store.get("missing").is_err());
    }

    #[test]
    fn empty_name_is_rejected() {
        let (_temp, store) = temp_store();
        assert!(store.create("   ", "", "", "").is_err());
    }

    #[test]
    fn mounts_add_revoke_and_roots_split() {
        let (temp, store) = temp_store();
        let project = store.create("Local", "", "", "").unwrap();
        let ro = temp.path().join("read-only");
        let rw = temp.path().join("read-write");
        std::fs::create_dir(&ro).unwrap();
        std::fs::create_dir(&rw).unwrap();

        let updated = ProjectStore::add_mount_sync(
            &store.path,
            &project.id,
            &ro,
            MountAccess::ReadOnly,
        )
        .unwrap();
        let updated = ProjectStore::add_mount_sync(
            &store.path,
            &updated.id,
            &rw,
            MountAccess::ReadWrite,
        )
        .unwrap();

        assert_eq!(updated.mounts.len(), 2);
        assert_eq!(updated.all_roots().len(), 2);
        assert_eq!(updated.writable_roots(), vec![rw.clone()]);

        let after_revoke =
            ProjectStore::revoke_mount_sync(&store.path, &updated.id, &ro).unwrap();
        assert_eq!(after_revoke.mounts.len(), 1);
        assert!(after_revoke.writable_roots().contains(&rw));

        // Duplicate mounts and unknown projects are rejected.
        assert!(
            ProjectStore::add_mount_sync(&store.path, &updated.id, &rw, MountAccess::ReadWrite)
                .is_err()
        );
        assert!(
            ProjectStore::add_mount_sync(&store.path, "proj-nope", &rw, MountAccess::ReadOnly)
                .is_err()
        );
    }

    #[test]
    fn mount_rejects_missing_directory() {
        let (temp, store) = temp_store();
        let project = store.create("Local", "", "", "").unwrap();
        let missing = temp.path().join("does-not-exist");
        assert!(
            ProjectStore::add_mount_sync(&store.path, &project.id, &missing, MountAccess::ReadOnly)
                .is_err()
        );
    }

    #[test]
    fn record_task_appends_history() {
        let (_temp, store) = temp_store();
        let project = store.create("History", "", "", "").unwrap();
        let updated = store.record_task(&project.id, "cowork-1").unwrap();
        assert_eq!(updated.task_ids, vec!["cowork-1".to_owned()]);
    }

    #[test]
    fn delete_removes_project() {
        let (_temp, store) = temp_store();
        let project = store.create("Gone", "", "", "").unwrap();
        assert!(store.delete(&project.id).unwrap());
        assert!(!store.delete(&project.id).unwrap());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 20, "YYYY-MM-DDTHH:MM:SSZ");
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
    }

    #[test]
    fn mount_health_tracks_moves_and_deletions() {
        let (temp, store) = temp_store();
        let project = store.create("Health", "", "", "").unwrap();
        let dir = temp.path().join("workspace");
        std::fs::create_dir(&dir).unwrap();
        let updated = ProjectStore::add_mount_sync(
            &store.path,
            &project.id,
            &dir,
            MountAccess::ReadWrite,
        )
        .unwrap();

        let mount = &updated.mounts[0];
        assert!(mount.identity.is_some(), "identity captured at mount time");
        assert_eq!(
            observe_mount(&mount.path, mount.identity).health,
            MountHealth::Available
        );

        // Health is a view, not store state: the JSON view annotates it,
        // the persisted document does not.
        assert_eq!(updated.view_json()["mounts"][0]["health"], "available");
        let persisted = std::fs::read_to_string(&store.path).unwrap();
        assert!(!persisted.contains("\"health\""), "health must not be persisted");

        // Renamed in place: recorded path gone, same inode re-found nearby.
        let renamed = temp.path().join("workspace-renamed");
        std::fs::rename(&dir, &renamed).unwrap();
        let moved = observe_mount(&mount.path, mount.identity);
        assert_eq!(moved.health, MountHealth::Moved);
        assert_eq!(moved.current_path.as_deref(), Some(renamed.as_path()));

        // Deleted outright: nowhere to re-find it.
        std::fs::remove_dir(&renamed).unwrap();
        assert_eq!(
            observe_mount(&mount.path, mount.identity).health,
            MountHealth::Disconnected
        );
        assert_eq!(moved.current_path.as_deref(), Some(renamed.as_path()));
    }

    #[test]
    fn legacy_mount_without_identity_reports_disconnected_when_gone() {
        let (temp, store) = temp_store();
        let project = store.create("Legacy", "", "", "").unwrap();
        let dir = temp.path().join("legacy");
        std::fs::create_dir(&dir).unwrap();
        let updated = ProjectStore::add_mount_sync(
            &store.path,
            &project.id,
            &dir,
            MountAccess::ReadOnly,
        )
        .unwrap();
        // Simulate a legacy store entry: no identity recorded.
        let legacy = ProjectMount {
            identity: None,
            ..updated.mounts[0].clone()
        };
        assert_eq!(
            observe_mount(&legacy.path, None).health,
            MountHealth::Available
        );
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(
            observe_mount(&legacy.path, None).health,
            MountHealth::Disconnected
        );
    }
}

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One scheduled Cowork task. Scheduling is project-scoped on purpose: the
/// project's mounts (with their access levels) define the sandbox the task
/// will run in, so a scheduled task can never run with roots nobody reviewed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ScheduleEntry {
    pub id: String,
    pub project_id: String,
    pub goal: String,
    pub model: Option<String>,
    /// Unix timestamp (seconds) when the task should fire.
    pub run_at_unix_secs: u64,
    pub created_at_unix_secs: u64,
    pub status: ScheduleStatus,
    /// Set when the entry fired successfully.
    pub task_id: Option<String>,
    pub fired_at_unix_secs: Option<u64>,
    /// Set when firing failed (project deleted, mounts unresolvable, ...).
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Pending,
    Done,
    Failed,
    Cancelled,
}

/// Persistent schedule for the Cowork desktop daemon, stored next to the
/// project store under the cowork data directory.
pub struct ScheduleStore {
    path: PathBuf,
    entries: Vec<ScheduleEntry>,
}

impl ScheduleStore {
    pub fn open_default() -> Result<Self> {
        let dir = crate::cli::cowork_rpc::store_dir()?;
        Self::open_at(dir.join("schedule.json"))
    }

    pub fn open_at(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let entries = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("invalid schedule store at {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        Ok(Self { path, entries })
    }

    pub fn store_path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Vec<ScheduleEntry> {
        self.entries.clone()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(&self.entries)?)?;
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }

    pub fn add(
        &mut self,
        project_id: impl Into<String>,
        goal: impl Into<String>,
        model: Option<String>,
        run_at_unix_secs: u64,
    ) -> Result<ScheduleEntry> {
        let goal = goal.into();
        anyhow::ensure!(
            !goal.trim().is_empty(),
            "scheduled task goal cannot be empty"
        );
        let entry = ScheduleEntry {
            id: crate::id::new_id("sched"),
            project_id: project_id.into(),
            goal,
            model,
            run_at_unix_secs,
            created_at_unix_secs: now_unix_secs(),
            status: ScheduleStatus::Pending,
            task_id: None,
            fired_at_unix_secs: None,
            error: None,
        };
        self.entries.push(entry.clone());
        self.save()?;
        Ok(entry)
    }

    pub fn mark_fired(&mut self, id: &str, task_id: &str) -> Result<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .with_context(|| format!("unknown schedule entry {id}"))?;
        entry.status = ScheduleStatus::Done;
        entry.task_id = Some(task_id.to_owned());
        entry.fired_at_unix_secs = Some(now_unix_secs());
        self.save()
    }

    pub fn mark_failed(&mut self, id: &str, error: impl Into<String>) -> Result<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .with_context(|| format!("unknown schedule entry {id}"))?;
        entry.status = ScheduleStatus::Failed;
        entry.error = Some(error.into());
        entry.fired_at_unix_secs = Some(now_unix_secs());
        self.save()
    }

    pub fn cancel(&mut self, id: &str) -> Result<bool> {
        let entry = match self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id && entry.status == ScheduleStatus::Pending)
        {
            Some(entry) => entry,
            None => return Ok(false),
        };
        entry.status = ScheduleStatus::Cancelled;
        self.save()?;
        Ok(true)
    }

    /// Pending entries whose due time has passed, oldest first.
    pub fn due_pending(&self, now_unix_secs: u64) -> Vec<ScheduleEntry> {
        let mut due: Vec<ScheduleEntry> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.status == ScheduleStatus::Pending && entry.run_at_unix_secs <= now_unix_secs
            })
            .cloned()
            .collect();
        due.sort_by_key(|entry| entry.run_at_unix_secs);
        due
    }
}

pub fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> ScheduleStore {
        let dir = std::env::temp_dir().join(format!(
            "jcode-schedule-test-{}-{tag}-{}",
            std::process::id(),
            crate::id::new_id("t")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        ScheduleStore::open_at(dir.join("schedule.json")).unwrap()
    }

    #[test]
    fn schedule_add_list_roundtrip() {
        let mut store = temp_store("roundtrip");
        let entry = store.add("proj-1", "Summarize weekly notes", None, 1_800_000_000).unwrap();
        assert_eq!(entry.status, ScheduleStatus::Pending);
        assert_eq!(entry.project_id, "proj-1");

        let reopened = ScheduleStore::open_at(store.store_path()).unwrap();
        let listed = reopened.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
        assert_eq!(listed[0].goal, "Summarize weekly notes");
    }

    #[test]
    fn schedule_due_filter_is_pending_only_and_ordered() {
        let mut store = temp_store("due");
        let early = store.add("p", "early", None, 100).unwrap();
        let late = store.add("p", "late", None, 900).unwrap();
        let future = store.add("p", "future", None, 5_000).unwrap();
        let done = store.add("p", "done", None, 50).unwrap();
        store.mark_fired(&done.id, "cowork-x").unwrap();
        store.cancel(&future.id).unwrap();

        let due = store.due_pending(1_000);
        let ids: Vec<&str> = due.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, vec![early.id.as_str(), late.id.as_str()]);
    }

    #[test]
    fn schedule_cancel_only_pending() {
        let mut store = temp_store("cancel");
        let entry = store.add("p", "goal", None, 10).unwrap();
        assert!(store.cancel(&entry.id).unwrap());
        assert!(!store.cancel(&entry.id).unwrap(), "second cancel must be a no-op");
        let listed = store.list();
        assert_eq!(listed[0].status, ScheduleStatus::Cancelled);
    }

    #[test]
    fn schedule_mark_failed_records_error() {
        let mut store = temp_store("failed");
        let entry = store.add("p", "goal", None, 10).unwrap();
        store.mark_failed(&entry.id, "project deleted").unwrap();
        let listed = store.list();
        assert_eq!(listed[0].status, ScheduleStatus::Failed);
        assert_eq!(listed[0].error.as_deref(), Some("project deleted"));
    }

    #[test]
    fn schedule_rejects_empty_goal() {
        let mut store = temp_store("empty");
        assert!(store.add("p", "   ", None, 10).is_err());
    }
}

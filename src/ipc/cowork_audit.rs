//! Append-only audit ledger for cowork security-relevant events.
//!
//! Every approval decision (stored rule, read-only policy auto-allow, or
//! explicit user choice), permission-rule change, approval-policy change,
//! and project/mount lifecycle event is recorded to
//! `<store-dir>/audit.jsonl`. The file is bounded: once it grows past
//! [`MAX_LEDGER_BYTES`] the oldest lines are rewritten away, keeping the
//! newest [`KEEP_ON_ROTATE`] entries.
//!
//! Recording is best-effort by design — a ledger failure is logged and
//! swallowed so it can never block or fail the decision it is auditing.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The data directory for the ledger file. Mirrors the cowork settings
/// store so tests can point `JCODE_COWORK_STORE_DIR` at a temp dir.
fn ledger_path() -> Result<PathBuf> {
    Ok(crate::cli::cowork_rpc::store_dir()?.join("audit.jsonl"))
}

/// What happened, in machine-readable form. The string form is what lands
/// in the ledger and what `audit.query` filters on — new kinds may appear
/// in files written by newer builds, so queries match strings, not enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditEventKind {
    /// A stored permission rule allowed the tool call before any prompt.
    RuleAllow,
    /// A stored permission rule denied the tool call before any prompt.
    RuleDeny,
    /// The read-only approval policy ran the tool without prompting.
    PolicyAllow,
    /// The user explicitly allowed a pending approval.
    UserAllow,
    /// The user explicitly rejected a pending approval.
    UserDeny,
    /// The approval surface disconnected before a decision arrived.
    PendingExpired,
    /// The call was refused outright because no approval surface was
    /// connected at all.
    BlockedNoClient,
    /// The approval policy was switched (`always_ask` /
    /// `auto_allow_readonly`).
    PolicyChanged,
    /// A permission rule was stored or replaced.
    RuleAdded,
    /// A permission rule was removed.
    RuleRemoved,
    /// A project was created.
    ProjectCreated,
    /// A project was deleted.
    ProjectDeleted,
    /// A folder was mounted into a project.
    MountAdded,
    /// A project mount was revoked.
    MountRevoked,
    /// A finished task was forked into a new resumable task.
    TaskForked,
    /// A finished task's conversation was rewound to an earlier message.
    TaskRewound,
}

impl AuditEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditEventKind::RuleAllow => "rule_allow",
            AuditEventKind::RuleDeny => "rule_deny",
            AuditEventKind::PolicyAllow => "policy_allow",
            AuditEventKind::UserAllow => "user_allow",
            AuditEventKind::UserDeny => "user_deny",
            AuditEventKind::PendingExpired => "pending_expired",
            AuditEventKind::BlockedNoClient => "blocked_no_client",
            AuditEventKind::PolicyChanged => "policy_changed",
            AuditEventKind::RuleAdded => "rule_added",
            AuditEventKind::RuleRemoved => "rule_removed",
            AuditEventKind::ProjectCreated => "project_created",
            AuditEventKind::ProjectDeleted => "project_deleted",
            AuditEventKind::MountAdded => "mount_added",
            AuditEventKind::MountRevoked => "mount_revoked",
            AuditEventKind::TaskForked => "task_forked",
            AuditEventKind::TaskRewound => "task_rewound",
        }
    }
}

/// One ledger row. Identity fields are `Option`s: a policy change has no
/// tool, a project rename has no task, and the GUI renders whatever is
/// present. `project_name` is denormalized at record time so entries stay
/// readable after the project is deleted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    /// Unique row id (monotonic nanosecond stamp).
    pub id: String,
    /// Unix seconds when the event was recorded.
    pub ts: u64,
    /// [`AuditEventKind::as_str`] of what happened.
    pub kind: String,
    /// The tool being approved, when the event is about a tool call.
    pub tool: Option<String>,
    /// The task the call belonged to, when known.
    pub task_id: Option<String>,
    /// The project the task or event is scoped to, when known.
    pub project_id: Option<String>,
    /// The project's name at record time (survives project deletion).
    pub project_name: Option<String>,
    /// Human-readable specifics: which rule, which path, which transition.
    pub detail: Option<String>,
}

/// The ledger grows until this many bytes, then rotates.
const MAX_LEDGER_BYTES: u64 = 1_048_576;
/// Entries kept on rotation — roughly the last month of heavy use.
const KEEP_ON_ROTATE: usize = 1000;
/// Hard ceiling for one `audit.query` response.
const MAX_QUERY_LIMIT: usize = 1000;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_id() -> String {
    format!(
        "audit-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// Append one entry, rotating the file first if it has grown past the cap.
/// Every failure is logged and swallowed: auditing must never become the
/// reason a decision fails.
pub fn record(
    kind: AuditEventKind,
    tool: Option<&str>,
    task_id: Option<&str>,
    project_id: Option<&str>,
    project_name: Option<&str>,
    detail: Option<String>,
) {
    let entry = AuditEntry {
        id: now_id(),
        ts: now_secs(),
        kind: kind.as_str().to_string(),
        tool: tool.map(str::to_owned),
        task_id: task_id.map(str::to_owned),
        project_id: project_id.map(str::to_owned),
        project_name: project_name.map(str::to_owned),
        detail,
    };
    let path = match ledger_path() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!("audit ledger unavailable: {error}");
            return;
        }
    };
    if let Err(error) = append_entry(&path, &entry) {
        tracing::warn!("could not append audit entry: {error}");
        return;
    }
    if let Err(error) = rotate_if_needed(&path, MAX_LEDGER_BYTES, KEEP_ON_ROTATE) {
        tracing::warn!("could not rotate audit ledger: {error}");
    }
}

fn append_entry(path: &Path, entry: &AuditEntry) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(entry).unwrap_or_default())
}

/// Rewrite the ledger keeping only its newest `keep` entries once it
/// exceeds `max_bytes` (single-writer daemon, so read-truncate-write is
/// safe without a lock file).
pub(crate) fn rotate_if_needed(path: &Path, max_bytes: u64, keep: usize) -> std::io::Result<()> {
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size <= max_bytes {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= keep {
        return Ok(());
    }
    let kept = &lines[lines.len() - keep..];
    let temp = path.with_extension("jsonl.tmp");
    std::fs::write(&temp, format!("{}\n", kept.join("\n")))?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Newest-first query over the ledger. Missing file means an empty ledger,
/// not an error; malformed lines are skipped so one bad write cannot hide
/// the rest of the history.
pub fn query(
    project_id: Option<&str>,
    task_id: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<AuditEntry>> {
    let path = ledger_path()?;
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut entries: Vec<AuditEntry> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    entries.retain(|entry| {
        project_id.is_none_or(|scope| entry.project_id.as_deref() == Some(scope))
            && task_id.is_none_or(|scope| entry.task_id.as_deref() == Some(scope))
            && kind.is_none_or(|scope| entry.kind == scope)
    });
    entries.reverse();
    let limit = limit.min(MAX_QUERY_LIMIT);
    entries.truncate(limit);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::cowork_rpc::test_support::lock_store_env;

    fn set_store_dir(dir: &Path) {
        // SAFETY: the crate-wide STORE_ENV_LOCK is held, so no other thread
        // reads the environment while it mutates.
        unsafe { std::env::set_var("JCODE_COWORK_STORE_DIR", dir) };
    }

    #[test]
    fn record_and_query_roundtrip_filters_newest_first() {
        let _env = lock_store_env();
        let directory = tempfile::tempdir().unwrap();
        set_store_dir(directory.path());

        record(AuditEventKind::ProjectCreated, None, None, Some("p1"), Some("Reports"), None);
        record(
            AuditEventKind::PolicyAllow,
            Some("webfetch"),
            Some("t1"),
            Some("p1"),
            Some("Reports"),
            None,
        );
        record(
            AuditEventKind::UserDeny,
            Some("bash"),
            Some("t2"),
            None,
            None,
            Some("user rejected".to_string()),
        );

        let all = query(None, None, None, 100).unwrap();
        assert_eq!(all.len(), 3, "every entry is returned");
        assert_eq!(all[0].kind, "user_deny", "newest entry first");
        assert_eq!(all[2].kind, "project_created", "oldest entry last");
        assert_eq!(all[1].project_name.as_deref(), Some("Reports"));

        let by_project = query(Some("p1"), None, None, 100).unwrap();
        assert_eq!(by_project.len(), 2);
        assert!(by_project.iter().all(|entry| entry.project_id.as_deref() == Some("p1")));

        let by_task = query(None, Some("t2"), None, 100).unwrap();
        assert_eq!(by_task.len(), 1);
        assert_eq!(by_task[0].tool.as_deref(), Some("bash"));

        let by_kind = query(None, None, Some("policy_allow"), 100).unwrap();
        assert_eq!(by_kind.len(), 1);

        let limited = query(None, None, None, 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].kind, "user_deny", "limit keeps the newest");
    }

    #[test]
    fn missing_ledger_reads_empty() {
        let _env = lock_store_env();
        let directory = tempfile::tempdir().unwrap();
        set_store_dir(directory.path());
        assert!(query(None, None, None, 50).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let _env = lock_store_env();
        let directory = tempfile::tempdir().unwrap();
        set_store_dir(directory.path());
        record(
            AuditEventKind::RuleAdded,
            Some("bash"),
            None,
            None,
            None,
            Some("global allow".to_string()),
        );
        let path = ledger_path().unwrap();
        let polluted = format!(
            "not json at all\n{{\"broken\": true}}\n{}\n",
            std::fs::read_to_string(&path).unwrap().trim_end()
        );
        std::fs::write(&path, polluted).unwrap();
        let entries = query(None, None, None, 100).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "rule_added");
    }

    #[test]
    fn rotation_keeps_the_newest_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audit.jsonl");
        let mut body = String::new();
        for index in 0..10 {
            let entry = AuditEntry {
                id: format!("audit-{index}"),
                ts: 1000 + index,
                kind: "user_allow".to_string(),
                tool: None,
                task_id: None,
                project_id: None,
                project_name: None,
                detail: None,
            };
            body.push_str(&serde_json::to_string(&entry).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();

        rotate_if_needed(&path, 100, 4).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let ids: Vec<String> = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEntry>(line).ok())
            .map(|entry| entry.id)
            .collect();
        assert_eq!(ids, vec!["audit-6", "audit-7", "audit-8", "audit-9"], "newest tail kept in order");
    }
}

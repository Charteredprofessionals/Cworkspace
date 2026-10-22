# Technical Design: jcode-Cowork Native Windows Engine & GUI

## 1. System Architecture Overview
The architecture decouples `jcode`'s terminal frontend (`ratatui`) from its core Rust daemon and replaces it with a **Tauri v2 + React + Tailwind CSS** desktop application communicating over a native Windows Named Pipe (`\\.\pipe\jcode-cowork`).
+-----------------------------------------------------------------------+
|                   TAURI v2 + REACT DESKTOP GUI                        |
|  [Folder Mount Manager]   [Multi-Agent Step Stream]  [Artifact Viewer]|
+-----------------------------------+-----------------------------------+
| JSON-RPC 2.0 over Named Pipe
| (\.\pipe\jcode-cowork)
+-----------------------------------v-----------------------------------+
|                     jcode RUST DAEMON (MODIFIED)                      |
|                                                                       |
|  +-----------------------+    +------------------------------------+  |
|  | Root Coordinator      |--->| Swarm Worker Pool (~3.1MB/session) |  |
|  | (Knowledge Persona)   |    +-----------------+------------------+  |
|  +-----------------------+                      |                     |
|                                                 v                     |
|  +-----------------------------------------------------------------+  |
|  |            SECURITY & SANDBOX MIDDLEWARE (NEW)                  |  |
|  |  1. PathJailValidator (UNC canonicalization + prefix check)     |  |
|  |  2. Win32JobSandbox (CreateJobObjectW + KillOnJobClose)         |  |
|  |  3. ToolApprovalGate (Async oneshot channel to GUI)             |  |
|  +------------------------------+----------------------------------+  |
|                                 |                                     |
|        +------------------------+------------------------+            |
|        v                        v                        v            |
|  +------------+          +--------------+         +--------------+    |
|  | File/Shell |          | Office/Doc   |         | Computer Use |    |
|  | Tools      |          | Engine (COM/ |         | (uiautomation|    |
|  | (PowerShell|          | calamine/pdf)|         | + xcap/enigo)|    |
|  +------------+          +--------------+         +--------------+    |
+-----------------------------------------------------------------------+


## 2. Sequence Diagram: Sandboxed Multi-Agent Task Execution

User          Cowork GUI (Tauri)       jcode Daemon (IPC)       Security Middleware       Swarm Worker
|                    |                        |                         |                     |
|--1. Mount Folder-->|                        |                         |                     |
|  & Submit Goal     |--2. start_task(goal,-->|                         |                     |
|                    |     allowed_roots)     |--3. Register Roots----->|                     |
|                    |                        |--4. Spawn Worker------->|-------------------->|
|                    |                        |                         |<--5. ToolCall(write)|
|                    |                        |                         |--6. Canonicalize &  |
|                    |                        |                         |     Check Prefix    |
|                    |<--7. Event: ToolRunning|-------------------------|                     |
|                    |                        |                         |--8. Execute in Win32|
|                    |                        |                         |     Job Object----->|
|                    |<--9. Event: Artifact---|-------------------------|<--10. Success-------|
|<--11. Render XLSX--|      Created           |                         |                     |


## 3. Core Rust Data Models & Interfaces (`src/sandbox/mod.rs`)

### 3.1 Path Jail Enforcer
```rust
use std::path::{Path, PathBuf};

pub struct PathJail {
    allowed_roots: Vec<PathBuf>, // Stored as canonicalized Windows UNC paths (\\?\C:\...)
}

impl PathJail {
    pub fn new(roots: Vec<PathBuf>) -> std::io::Result<Self> {
        let canonical_roots = roots
            .into_iter()
            .map(|r| std::fs::canonicalize(r))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { allowed_roots: canonical_roots })
    }

    pub fn verify_path(&self, candidate: &Path) -> Result<PathBuf, String> {
        // If creating a new file, canonicalize the parent directory first
        let target = if candidate.exists() {
            std::fs::canonicalize(candidate).map_err(|e| e.to_string())?
        } else {
            let parent = candidate.parent().ok_or("Invalid parent directory")?;
            let canon_parent = std::fs::canonicalize(parent).map_err(|e| e.to_string())?;
            canon_parent.join(candidate.file_name().ok_or("Invalid file name")?)
        };

        if self.allowed_roots.iter().any(|root| target.starts_with(root)) {
            Ok(target)
        } else {
            Err(format!("SECURITY_VIOLATION: Path {:?} is outside mounted workspace roots.", candidate))
        }
    }
}
3.2 Win32 Job Object Wrapper (src/sandbox/win_job.rs)
Uses windows-sys (Win32::System::JobObjects) to bind every spawned PowerShell/CLI process to a kernel-managed job object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE and JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION. Dropping the WinJobGuard struct in Rust automatically closes the handle and kills the entire process tree.

3.3 Document Context Trimmer (src/context/doc_outline.rs)
Extends jcode's AST code trimmer so non-code files return lightweight schemas first:

.xlsx / .csv: Uses calamine to return sheet names, row counts, column headers, and top 3 sample rows.

.docx: Parses word/document.xml to extract Heading 1–3 hierarchy and table summaries.

.pdf: Extracts bookmark outline, page count, and first-page abstract.

4. Correctness Properties (For Property-Based Testing)
Property 1 (Path Traversal Invariant): For any arbitrary string input containing combinations of ..\, /, \, NTFS alternate data streams (:), or symlink hops, PathJail::verify_path MUST never return Ok(path) where path does not share an exact canonical prefix with at least one allowed_root.

Property 2 (Zero Orphan Processes): For any spawned shell command that recursively forks child processes (powershell -> cmd -> ping), dropping WinJobGuard MUST result in 0 surviving child PIDs within 100ms.
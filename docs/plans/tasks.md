## 3. File 3: `.kiro/specs/jcode-cowork-engine/tasks.md`
Kiro executes tasks in **waves**. Tasks inside the same wave have no cross-dependencies and can be run in parallel by separate sub-agents.

```markdown
# Implementation Tasks: jcode-Cowork Engine & GUI

## Wave 1: Core Engine Security & IPC Foundation (Parallel Backend)
- [ ] **Task 1.1: Implement `PathJail` Middleware (`src/sandbox/path_jail.rs`)**
  - **Maps to:** REQ-JAIL-01, REQ-JAIL-02, REQ-JAIL-03
  - **Action:** Create `PathJail` struct with Windows UNC canonicalization (`std::fs::canonicalize`). Hook `PathJail::verify_path` into `jcode`'s central tool dispatcher before `read`, `write`, `edit`, `glob`, and `grep` execute.
  - **Verification:** Write property-based tests (`proptest`) asserting `..\` traversal and junction escapes outside a temp directory always return `Err`.

- [ ] **Task 1.2: Implement Win32 Job Object Sandbox (`src/sandbox/win_job.rs`)**
  - **Maps to:** REQ-PROC-01, REQ-PROC-02, REQ-PROC-03
  - **Action:** Add `windows-sys` with `Win32_System_JobObjects` and `Win32_System_Threading` features. Wrap `jcode`'s PowerShell/bash command runner so every spawned `tokio::process::Command` is assigned to a `WinJobGuard` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
  - **Verification:** Spawn a PowerShell script that launches a background child process, abort the task, and assert child PID termination.

- [ ] **Task 1.3: Build JSON-RPC Named Pipe Server & Approval Gate (`src/ipc/cowork_rpc.rs`)**
  - **Maps to:** REQ-IPC-01, REQ-IPC-02, REQ-IPC-03
  - **Action:** Expose a `--cowork-daemon` CLI flag in `jcode` that binds `\\.\pipe\jcode-cowork`. Stream swarm worker status events and implement a `tokio::sync::oneshot` approval channel that pauses destructive tools until `approve_tool_call(id, approved)` is received.
  - **Verification:** Connect via a mock named-pipe client, send a task request, intercept an approval prompt, and verify resumption.

## Wave 2: Knowledge-Work & Computer Use Tooling (Parallel Tool Modules)
- [ ] **Task 2.1: Document Outline Trimmer & Office Tooling (`src/tools/office_docs.rs`)**
  - **Maps to:** REQ-DOC-01, REQ-DOC-02
  - **Action:** Integrate `calamine` (Excel), `rust_xlsxwriter` (Excel generation), and `docx-rs` / PowerShell COM bridges into `jcode`'s tool registry. Emit `ArtifactCreated { path, mime_type }` events whenever an Office or PDF file is written.
  - **Verification:** Run tool unit tests that parse a multi-sheet `.xlsx` schema and generate a formatted `.docx` report.

- [ ] **Task 2.2: Native Windows Computer Use Module (`src/tools/computer_use_win.rs`)**
  - **Maps to:** REQ-GUI-AUTO-01, REQ-GUI-AUTO-02
  - **Action:** Integrate `xcap` (screen capture), `enigo` (input injection), and `uiautomation` (Win32 UI tree inspector). Register a global hotkey listener (`Ctrl+Alt+Backspace`) that flips an `AtomicBool` kill-switch to immediately halt input synthesis.
  - **Verification:** Query the Windows Calculator or Notepad UI tree in a test and verify coordinate extraction and kill-switch interruption.

- [ ] **Task 2.3: Coordinator Knowledge-Work Persona Refactor (`src/prompts/cowork_persona.rs`)**
  - **Maps to:** REQ-DOC-01, REQ-UI-01
  - **Action:** Add a Cowork mode system prompt profile that shifts the root coordinator and swarm sub-agents from code-diff workflows to document synthesis, data auditing, step logging, and artifact generation.
  - **Verification:** Verify prompt switching when `--cowork-daemon` is active.

## Wave 3: Tauri v2 + React Cowork Desktop GUI (Parallel Frontend)
- [ ] **Task 3.1: Scaffold Tauri v2 Shell & Named Pipe Bridge (`gui/src-tauri/`)**
  - **Maps to:** REQ-IPC-01, REQ-UI-01
  - **Action:** Initialize Tauri v2 project inside `gui/`. Write Tauri Rust commands (`mount_folder`, `start_cowork_task`, `stop_task`, `respond_approval`) that proxy directly to `\\.\pipe\jcode-cowork` and forward daemon events to the React webview.
  - **Verification:** `cargo check` in `gui/src-tauri` passes and connects to the running `jcode` daemon.

- [ ] **Task 3.2: Build Cowork 3-Pane React UI (`gui/src/`)**
  - **Maps to:** REQ-UI-01, REQ-IPC-02, REQ-IPC-03
  - **Action:** Build the Left Workspace Sidebar (native folder picker + mounted folder pills), Center Execution Feed (goal bar, live swarm sub-agent tree, inline Allow/Deny permission cards), and Right Artifact Preview Drawer (renders Markdown, HTML previews, and XLSX tables).
  - **Verification:** End-to-end task run in the GUI mounts a folder, displays live sub-agent progress, prompts for permission on file deletion, and previews a generated `.xlsx` file.
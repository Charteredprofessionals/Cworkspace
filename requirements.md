# Requirements Specification: jcode-Cowork Native Windows Engine & GUI

## 1. Overview
Transform `jcode` from a terminal-centric coding harness into a native Windows multi-agent knowledge-work engine ("jcode-Cowork") with a Tauri v2 React desktop GUI, native Win32 sandboxing (zero Linux VM overhead), Office COM/document tooling, and Windows UI Automation.

## 2. User Stories & EARS Acceptance Criteria

### 2.1 Headless JSON-RPC / Named Pipe Bridge
**User Story:** As a desktop user, I want a responsive graphical interface that communicates seamlessly with the background `jcode` daemon without opening a terminal window.
- **REQ-IPC-01:** WHEN the desktop GUI launches, THE SYSTEM SHALL connect to the `jcode` background daemon over a native Windows Named Pipe (`\\.\pipe\jcode-cowork`) or local WebSocket.
- **REQ-IPC-02:** WHEN a task is executing across a multi-agent swarm, THE SYSTEM SHALL stream real-time structured telemetry events (`AgentSpawned`, `ToolInvoked`, `ArtifactCreated`, `ApprovalRequired`, `TaskCompleted`) to the GUI at less than 50ms latency.
- **REQ-IPC-03:** IF a tool requires elevated user permission (such as deleting files or sending external network payloads), THEN THE SYSTEM SHALL pause the worker coroutine and emit an `ApprovalRequired` event to the GUI until the user approves or rejects.

### 2.2 Application-Level Path Jail (Zero-VM File Sandbox)
**User Story:** As a security-conscious user, I want the AI agents restricted strictly to the workspace folders I explicitly grant in the UI so my personal system files remain untouched.
- **REQ-JAIL-01:** WHEN a user mounts one or more folders in the GUI, THE SYSTEM SHALL register those canonicalized paths (`std::fs::canonicalize`) as the active `AllowedWorkspaceRoots` in the `jcode` session state.
- **REQ-JAIL-02:** WHEN any agent invokes a file tool (`read`, `write`, `edit`, `glob`, `grep`, `office_doc`), THE SYSTEM SHALL resolve symlinks, junctions, and `..\` relative segments and verify that the target path begins with an active `AllowedWorkspaceRoot`.
- **REQ-JAIL-03:** IF an agent attempts to access a file or directory outside `AllowedWorkspaceRoots`, THEN THE SYSTEM SHALL block execution immediately and return a structured `SecurityViolationError` to the LLM context.

### 2.3 Win32 Job Object Process Containment
**User Story:** As a Windows user, I want every shell command or script spawned by an agent to be strictly contained and automatically cleaned up so runaway processes never freeze my PC.
- **REQ-PROC-01:** WHEN the `jcode` daemon spawns a PowerShell or CLI child process, THE SYSTEM SHALL assign the process handle to a Win32 Job Object (`CreateJobObjectW`) configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
- **REQ-PROC-02:** WHEN a user clicks "Stop Task" in the GUI or a task times out, THE SYSTEM SHALL terminate the Win32 Job Object, immediately killing the root shell and all descendant child processes with zero orphan processes left in Windows Task Manager.
- **REQ-PROC-03:** WHEN spawning untrusted script processes, THE SYSTEM SHALL enforce working-directory confinement to the mounted workspace root and cap per-job memory allocation via `JOB_OBJECT_LIMIT_JOB_MEMORY`.

### 2.4 Knowledge-Work & Native Windows Document Tooling
**User Story:** As a knowledge worker, I want agents to inspect, create, and format Word, Excel, PowerPoint, and PDF files natively without losing formatting.
- **REQ-DOC-01:** WHEN an agent inspects a folder containing `.pdf`, `.docx`, `.xlsx`, or `.csv` files, THE SYSTEM SHALL extract structural metadata (headers, sheet names, column schemas, TOC) before loading raw text to preserve the context window.
- **REQ-DOC-02:** WHEN an agent generates or edits `.xlsx` or `.docx` deliverables, THE SYSTEM SHALL execute structured Rust document tools or Windows COM PowerShell bridges to produce valid native Office artifacts and emit an `ArtifactCreated` event to the GUI preview drawer.

### 2.5 Native Windows Computer Use ("Eyes & Hands")
**User Story:** As a user automating legacy desktop software, I want the agent to read Windows UI elements and control mouse/keyboard input safely.
- **REQ-GUI-AUTO-01:** WHEN an agent invokes the `computer_use` tool on Windows, THE SYSTEM SHALL capture the target window via `xcap` AND query the Win32 Accessibility tree (`uiautomation`) to provide both visual screenshots and exact UI control coordinates.
- **REQ-GUI-AUTO-02:** IF the user presses the global hardware panic hotkey (`Ctrl+Alt+Backspace`), THEN THE SYSTEM SHALL immediately halt all `enigo` mouse/keyboard synthesis and pause all active agents.

### 2.6 Claude Cowork-Style Desktop GUI
**User Story:** As a non-technical user, I want an outcome-focused desktop interface with a goal bar, mounted folder pills, live multi-agent step logs, and side-by-side artifact previews.
- **REQ-UI-01:** WHEN the user opens the application, THE SYSTEM SHALL render a three-pane Cowork layout: Left Sidebar (Workspace Folder Mounts & Task History), Center Pane (Outcome Prompt Bar & Collapsible Multi-Agent Step Feed), and Right Drawer (Live Artifact Preview for Markdown, PDF, XLSX tables, and DOCX).
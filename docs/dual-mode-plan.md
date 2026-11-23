# Cworkspace dual-mode plan — Knowledge Work ⇄ Software Engineering

Target (reference screenshot of Claude Cowork's General settings): an
**Interface Layout** choice between **Knowledge Work** ("simplified interface
for research, writing, and everyday tasks") and **Software Engineering** ("full
interface for coding, developer tools, and managing agents"), under a General
section that also covers **agent execution**, **queued message delivery**, and
**permissions**.

## Baseline after v0.1.4 (parity release)

The desktop already speaks the whole jcode daemon (protocol v3): every provider
(Accounts), MCP servers, scheduled tasks, transcripts, usage windows, the tool
catalog, approval modes, projects with read-only/read-write mounts, artifact
previews + downloads. But there is exactly **one** interface — the full
Software-Engineering one — and three General-style capabilities are missing
entirely: queued messages, per-tool permission rules, and background execution.

## Gap → improvement list (priority order)

### 1. Interface Layout setting (the mode switch itself) — v0.1.5, UI-only

- New local setting `interfaceLayout: "knowledge" | "software"` (localStorage,
  next to `reopenFolders`/`autoOpenArtifacts`/`defaultModel`), chosen on
  first run and switchable anytime in Settings → General.
- **Knowledge Work** renders a reduced app: composer (goal + project + folder
  mount, no model picker — the default model applies), plain-language feed
  ("Reading the spreadsheet…", not `ToolInvoked bash`), Artifacts panel as the
  primary surface, RECENT WORK, SCHEDULED, and Accounts simplified to
  "Connected services". Settings hides MCP internals, the tool catalog, and the
  Connection/transport facts; approvals read as permission cards ("Cworkspace
  wants to save a file in *Q4 Close* — Allow?").
- **Software Engineering** is today's UI, unchanged, plus the additions below.
- Effort ≈ 1 day; conditional rendering only, zero daemon changes.

### 2. Queued message delivery + follow-up turns — v0.1.5, daemon + UI

The biggest functional gap versus the screenshot: today a running task accepts
no input (composer disabled), and a finished task can only be restored into the
composer as a fresh goal.

- UI: while a task runs, the composer becomes a **message queue** — typed
  messages stack as chips under the feed; on completion the first queued
  message offers "Send as follow-up".
- Daemon: new `task.followup {task_id, message}` — the daemon already persists
  every task session, and the Agent has a `load_for_resume` path, so a
  follow-up is: resume that session under the same task root/mount jail, run
  one more turn (`run_once_capture`), stream the same `task_id`-tagged events
  to the same tab. Approvals re-gate per the active policy.
- Effort ≈ 2 days; reuses existing session-resume machinery.

### 3. Permission rules (per tool, per project) — v0.1.6, daemon + UI — SHIPPED

General's "permissions" implies more than the two global approval modes.

Shipped as `PermissionRule { tool, decision: allow|deny, scope: global|project }`
persisted in the cowork store (`permission_rules.json`); the approval gate
consults rules **before** the policy (project scope beats global, newest wins,
one rule per tool+scope). Approval cards gained "Always allow / Always block"
(remember scopes to the task's project when it has one, else global);
Settings → Permissions lists rules with an add/remove editor in both layouts.

- Daemon settings store gains `permissions: { always_allow: [...],
  per_project: { <project_id>: { allow: [...], ask: [...] } } }`; the approval
  gate consults the rules **before** prompting (allow → auto-approve, ask →
  prompt even in auto-allow mode).
- Approval cards grow "Always allow this tool in this project".
- Settings → Permissions: per-project editor with two lists (always allowed /
  always ask); Knowledge Work renders it as plain-language toggles.
- Effort ≈ 2 days.

### 4. Background execution + tray — v0.1.7 — SHIPPED

Today the daemon is a GUI child wrapped in a kill-on-close job object: closing
the window kills running tasks — acceptable for devs, hostile to "start a
report and walk away".

Shipped differently from the sketch (simpler, no setting): the job object
lost its kill-on-close flag entirely, so the daemon — and every running task
— survives GUI close, GUI crash, and OS logoff by default. Closing the
window hides to tray (Open Cworkspace / task count / Quit). Quit asks the
daemon to shut down gracefully (new `shutdown` RPC: aborts running tasks,
then the server loop exits after flushing the response); the quit path
retains a job/child kill fallback. Re-adoption needs no daemon-info file —
the pipe name is fixed, so the GUI's existing connect-before-spawn logic
attaches to the running daemon, and `connect_daemon` now reports
`{adopted, running_tasks}` for a frontend reattach notice. An idle janitor
exits a daemon left orphaned after ten consecutive quiet minutes (no client,
no tasks); installer hooks already taskkill jcode.exe for upgrades.

### 5. Streaming + narrative feed — protocol v4 — SHIPPED (v0.1.8)

- Stream token deltas as cowork events so answers type out live.
- Knowledge Work narration layer: map tool events to human sentences
  (office_docs → "Writing the report…", websearch → "Searching the web…").
- The sibling harness API already has streaming events; this is a protocol
  addition (`PROTOCOL_VERSION = 4`, additive like v3).

Shipped: `run_cowork_task` now drives the engine's streaming turn path
(`run_once_streaming_mpsc`) and forwards assistant text through a new
additive `CoworkEvent::TaskDelta { task_id, text, replace }` — TextReplace
and TextSuffixReplace are folded into full-replacement deltas so the
GUI's append-only narrative stays correct, and the accumulated text doubles
as the `TaskCompleted` summary (identical to what `run_once_capture`
returned). The GUI batches deltas into a ~120 ms flush, merging chunks into
the tab's last narrative entry (Pencil timeline symbol, purple); completion
flushes the buffer before the summary entry lands. Narration phrases
extended to swarm/batch/skill_manage/conversation_search/codemode/side
panel/mcp. 2 new tests (serde roundtrip + forwarder semantics), 381/381.

### 6. First-run onboarding — v0.1.5, UI-only

Layout choice → connect a provider (Accounts) → create/mount a project → first
goal. Every step is an existing dialog; it only needs sequencing.

### 7. Knowledge Work extras (backlog)

- **Recent documents** gallery: artifacts plus a bounded, mount-jailed
  directory listing RPC (`project.files {project_id, subpath?}`) so
  non-devs can open deliverables without knowing what a mount is.
- Goal templates for common knowledge work ("Draft a brief about…",
  "Summarize this folder", "Compare these two files").

## What does not change

The security model: capability-token IPC, loopback-only transport, PathJail
read/write jails, human-in-the-loop approvals (with the new rules layered on
top), kill-on-close until #4 opts out explicitly.

## Suggested release sequencing

| Release | Contents |
|---|---|
| v0.1.5 | #1 Interface Layout + #6 onboarding + #2 queued messages / follow-ups |
| v0.1.6 | #3 permission rules |
| v0.1.7 | #4 background execution + tray |
| v0.1.8 | #5 streaming (protocol v4) + narrative feed |

jcode itself needs no changes for #1/#6; #2/#3/#4/#5 are additive daemon RPC +
settings work, keeping the CLI and desktop on one capability core.

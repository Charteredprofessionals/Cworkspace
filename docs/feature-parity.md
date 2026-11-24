# jcode ⇄ Cworkspace desktop feature parity

Every jcode capability and where it lives in the desktop app. "Bridged" = usable
from the Cworkspace GUI (protocol v3, v0.1.4). "CLI-assisted" = one terminal
command, because the capability is inherently interactive or platform-bound.
Daemon-side enforcement is identical either way — the desktop talks to the same
`jcode --cowork-daemon` process the CLI builds on.

## Bridged in the desktop (v0.1.4)

| jcode capability | Desktop surface |
|---|---|
| Agent task runs (full tool registry) | Composer → Start task; feed shows agents, tool activity, approvals |
| All ~45 providers + model catalog auto-fetch | MODELS sidebar, Settings → default model, per-task model picker in the composer |
| Per-task instructions | Composer instructions field (ad-hoc) + project instructions (daemon-injected) |
| Scheduled work (`schedule` tool / ambient queue) | SCHEDULED sidebar section + Schedule dialog; daemon fires due entries every 30 s as project-scoped tasks (`schedule.create/list/cancel`) |
| MCP servers (`~/.jcode/mcp.json`) | Settings → MCP servers: list / add (stdio or http/sse) / remove; every desktop task now registers MCP tools exactly like `jcode run` does |
| Tool inventory (`--tools`, tool profiles) | Settings → Tools available to tasks (`tools.list`); read-only tools flagged |
| Approval modes | Settings → Capabilities (`settings.get/update`), enforced daemon-side |
| Session transcripts / history | RECENT WORK + per-task **View transcript** (`task.transcript`) |
| Usage limits (`jcode usage`) | Settings → Usage (`usage.report`: per-provider windows + Anthropic 5h/7d) |
| Auth status / API keys (`jcode login`, `auth import status`) | Settings → Accounts (`auth.status`, `auth.set_api_key`, `auth.clear`); OAuth/device flows remain CLI-assisted |
| Session/workspace roots | Projects with mounts (read-only/read-write), PathJail + write jail enforced in the daemon |
| Office authoring | `office_docs`: xlsx / docx / pptx / pdf |
| Artifact delivery + preview | Artifacts panel: inline md/pdf/xlsx/docx; pptx, text/code, and everything else via download |
| Read-only enforcement | Project mounts with access levels; writes into read-only roots denied daemon-side |
| Windows computer use | `computer_use` tool available in tasks (approval-gated) |
| Multi-task | Tabs, one task per tab, `task_id`-routed events |
| Project history | Task-count badges, project edit/delete, per-project default model |
| Streaming answers (protocol v4) | Feed types out live (`TaskDelta`), v0.1.8 |
| Mount health states | Available / moved / disconnected badges with re-find by (dev, ino), v0.1.9 |
| Dynamic tray menu | Running tasks by name, per-task focus/stop, 20 s rebuild, v0.1.9 |
| Feed replay on reattach | New tab reseeds from the persisted transcript, v0.1.9 |
| Audit ledger | Every gate decision, rule/policy change, and project/mount event recorded; AUDIT sidebar → Approval ledger (`audit.query`), v0.1.10 |
| Per-task display titles | Double-click a tab title; daemon persists it (`task.set_title`), tray + replay prefer it, v0.1.10 |
| Fork a finished task | **Fork task** on finished sessions: daemon copies the persisted session (child of the original, fork notice included) into a follow-up-ready new task with inherited mounts/project/model (`task.fork`), v0.1.11 |
| Rewind a conversation | **Rewind here** on every transcript message: drops everything after the chosen point, clears provider-resume state so context rebuilds locally (`task.rewind`), v0.1.11 |

## CLI-assisted (by design)

| Capability | Why | Command |
|---|---|---|
| OAuth / device-code logins (Claude, ChatGPT, Copilot, Gemini OAuth, Antigravity, …) | Browser + device flows | `jcode login <provider>` |
| MCP OAuth for remote servers | Browser flow | `jcode mcp login <server>` |
| Interactive TUI chat, `connect`, `replay`, video export | Terminal-native rendering | `jcode` |
| SSH attach to a remote daemon | Terminal transport | `jcode --ssh <host>` |
| ACP / api-bridge / debug socket | Editor + SDK integrations | `jcode acp` / `jcode api-bridge` |
| Memory CRUD, skills management, swarm control | In-agent tools + debug socket; surfaced through tasks | inside a task, or `jcode memory …` |
| Voice, dictation, companion, hotkeys, menubar, iOS pairing | Platform-bound (audio/menu surfaces) | `jcode companion`, `jcode dictate`, … |
| Cloud (Jade) sessions | Account-bound sync | `jcode cloud …` |
| Telemetry, browser setup, selfdev | Local machine configuration | `jcode telemetry …`, `jcode browser setup` |

## Not yet bridged (backlog)

- Ambient runner parity (the desktop scheduler is a separate project-scoped store, not the TUI ambient queue).
- Split-pane / parent-child workstreams.
- Connectors/OAuth (browser/app scopes) + browser/computer-use escalation UI.
- Members/sharing.
- macOS/Linux desktop builds (daemon already serves WebSocket IPC on non-Windows).

## Security model (unchanged)

Capability-token IPC (every method gated), loopback-only transport, PathJail
read/write jails per mount, human-in-the-loop approvals with the optional
read-only auto-allow, kill-on-close job object tying the daemon to the app.

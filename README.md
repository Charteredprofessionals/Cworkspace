<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/jcode-mark-inverse.svg">
  <img src="assets/brand/jcode-mark.svg" alt="jcode halftone donut logo" width="160">
</picture>

<br><br>

# Cworkspace

<p>
  <strong>A hardened fork of the jcode coding-agent CLI, shipped with the Cworkspace desktop companion.</strong><br>
  Run dozens of agents in parallel without melting your machine. More agents, less RAM, no lag.
</p>

[![Last Commit](https://badgen.net/github/last-commit/Charteredprofessionals/Cworkspace/master?icon=github)](https://github.com/Charteredprofessionals/Cworkspace/commits/master)
[![GitHub Stars](https://badgen.net/github/stars/Charteredprofessionals/Cworkspace?icon=github)](https://github.com/Charteredprofessionals/Cworkspace/stargazers)
[![GitHub Downloads](https://img.shields.io/github/downloads/Charteredprofessionals/Cworkspace/total?style=flat-square)](https://github.com/Charteredprofessionals/Cworkspace/releases)

[Windows installer](https://github.com/Charteredprofessionals/Cworkspace/actions/workflows/windows-installer.yml) · [Docs index](docs/README.md) · [Contributing](CONTRIBUTING.md)

</div>

## What is Cworkspace?

Cworkspace bundles two applications built from this repository:

- **jcode** — a Rust coding-agent CLI: parallel multi-model agent sessions, swarm coordination, memory, MCP tool servers, and a self-development mode where the agent can rebuild and reload its own binary. Providers include Claude (OAuth and API key), OpenAI / Codex (OAuth and API key), Gemini, Copilot, Bedrock, Cursor, OpenRouter, and any OpenAI-compatible endpoint.
- **Cworkspace** — a Tauri desktop GUI (`gui/`) that talks to a locally-running `jcode --cowork-daemon` over a capability-token-authenticated Windows named pipe. It surfaces running tasks, folder mounts, approvals, and artifacts.

This fork carries a security-hardening series on top of upstream jcode: authenticated daemon IPC, a human-in-the-loop approval gate for tool calls, computer-use gating with rate limits, PathJail sandboxing, and Windows job objects that kill runaway child processes. `docs/SAFETY_SYSTEM.md` is the reference.

## Installation

```bash
# macOS & Linux
curl -fsSL https://raw.githubusercontent.com/Charteredprofessionals/Cworkspace/master/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
git clone https://github.com/Charteredprofessionals/Cworkspace.git
cd Cworkspace
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
```

Prebuilt Windows installers (MSI/EXE) are built by the [Windows Installer workflow](https://github.com/Charteredprofessionals/Cworkspace/actions/workflows/windows-installer.yml) on every manual dispatch; download the artifacts from a run's page. See [docs/WINDOWS.md](docs/WINDOWS.md) for platform specifics.

## Building from source

```bash
# CLI (Rust workspace, ~90 crates; root package `jcode`)
cargo build

# Cworkspace GUI (Tauri v2 + React)
cd gui
npm ci
npm run tauri dev     # dev
npm run tauri build   # bundled desktop app
```

A Nix dev shell is available (`nix develop`, see [CONTRIBUTING.md](CONTRIBUTING.md)).

## Repository map

| Path | What it is |
|---|---|
| `src/` | CLI binary and command layer (`jcode`, `jcode --cowork-daemon`) |
| `crates/` | Rust workspace: providers, agent runtime, tools, TUI, IPC, security |
| `gui/` | Cworkspace desktop app (React + Tauri v2) |
| `scripts/` | Installers, release tooling, CI budget gates, dev helpers |
| `docs/` | Reference documentation (`docs/README.md` is the index) |
| `changelog/` | Structured per-version release notes consumed by release tooling |
| `telemetry-worker/` | Cloudflare Worker backing opt-in telemetry |
| `sdk/` | TypeScript/npm SDK |
| `ios/` | iOS remote-control companion app (`docs/IOS_APP.md`) |
| `tests/` | End-to-end test suites |

## Performance

### RAM per additional session (headless)

Swarm workers run headless, so this is the number that matters when you fan out agents.

| Tool | First session | Each additional session | Comparison (additional) |
|---|---|---|---|
| **jcode** | **32.9 MB** | **~6.4 MB** | baseline |
| **OpenCode** | 637.1 MB | ~5.9 MB | about the same |
| **pi** | 84.0 MB | ~79.6 MB | **12.4× more RAM** |
| **Claude Code** | 272.9 MB | ~169.3 MB | **26.5× more RAM** |
| **Codex CLI** | 398.6 MB | ~194.1 MB | **30.3× more RAM** |

Each session completes 5 real model turns with tool calls, then total PSS of every process is measured at 1, 5, and 10 concurrent sessions, averaged over two runs. Reproduce with [scripts/bench_headless_memory.py](scripts/bench_headless_memory.py).

### Time to first input

Time until typed text appears on screen, across 10 interactive launches.

| Tool | Time to first input | Comparison |
|---|---|---|
| **jcode** | **48.7 ms** | baseline |
| **pi** | 596.4 ms | **12.2× slower** |
| **Codex CLI** | 905.8 ms | **18.6× slower** |
| **OpenCode** | 1047.9 ms | **21.5× slower** |
| **Claude Code** | 3512.8 ms | **72.2× slower** |

## Swarm

Spawn two or more agents in the same repo, and they will automatically be managed by the server to allow native collaboration. When agent A edits a file that agent B has read (code shifting under its feet), the server notifies agent B. Agent B can ignore it if it is not relevant, or it can check the diff to make sure that it doesn't conflict. Each agent has messaging abilities, capable of DMing just one agent, broadcasting to all other agents hosted by the server, or just agents working in that repo. This allows you to spawn multiple sessions in the same repo, and have all conflicts automatically resolved.

Agents are also able to spawn their own swarms autonomously. They have a swarm tool which allows them to spawn in their own teammates to accomplish tasks in parallel. Doing so turns the main agent into a coordinator and the spawned agents into workers. Groups of agents, their messaging channels, their completion statuses, etc. are all automatically managed. This can be done headlessly or headed. Details: [docs/SWARM_ARCHITECTURE.md](docs/SWARM_ARCHITECTURE.md).

## Token optimization

### agentgrep

agentgrep is jcode's grep tool, built for agents. Each match comes back with the file's structure, like its functions and where they sit, so the agent can understand a file without reading all of it. The harness also adaptively trims results based on what the agent has already seen, so it never pays for the same code twice. This saves a lot of context.

## OAuth and Providers

Supports a wide range of providers, including Claude OAuth and OpenAI Codex OAuth. Model lists are fetched live from each provider's catalog endpoint (with a persisted cache and static fallback for offline use). Credential handling is documented in [docs/AUTH_CREDENTIAL_SOURCES.md](docs/AUTH_CREDENTIAL_SOURCES.md) and [OAUTH.md](OAUTH.md).

## Customizability / Self-Dev

Tell your jcode agent to enter self-dev mode, and it will start modifying its own source code. jcode is optimized to iterate on itself: there is significant infrastructure around self development, which allows it to edit, build, and test its own source code, then reload its own binary and continue work in your (potentially many) sessions, fully automatically. See [docs/AMBIENT_MODE.md](docs/AMBIENT_MODE.md) for the ambient loop.

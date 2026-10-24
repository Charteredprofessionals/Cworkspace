# AGENTS.md - jcode Production Application

## One-Paragraph App Summary
jcode is a blazing-fast TUI multi-model coding agent with swarm coordination, 30+ tools, and desktop integration. Provides an interactive agent session via `jcode run` with a long-lived daemon at `~/.jcode/builds/shared-server/jcode`.

## Documentation Index - Read This When...

| Doc | Read When |
|-----|-----------|
| `docs/system-map.md` | Overview of components and data flow |
| `docs/change-impact.md` | "If I change X, what else breaks?" |
| `docs/invariants-and-gotchas.md` | Non-obvious behavior, fragile areas |
| `docs/troubleshooting.md` | Symptom → cause → fix for common issues |
| `docs/data-model.md` | Schema entities, relationships, constraints |
| `docs/api-contracts.md` | Endpoints, auth, request/response shapes |
| `docs/config-and-env.md` | Env vars, feature flags, configuration |
| `docs/testing.md` | Test organization and verification |
| `docs/tech-debt.md` | Known shortcuts, TODOs, fragile code |
| `docs/open-questions.md` | Unknowns needing human confirmation |
| `docs/decisions/ADR-*.md` | Major architectural decisions |

## Hard Rules

- **Never** override `user.name` or `user.email` in commits with fabricated values
- **Always** use the user's configured Git identity
- **Do not** create a branch merely to start a task; work directly on master/main
- **Preserve** unrelated work when integrating PRs or branches
- **Never** require a maintainer-authored rewrite merely based on author identity
- **Always** repoint the shared daemon symlink after `cargo build -p jcode-tool-core` via `jcode self-dev --build`
- **Always** use `eprintln!` for throwaway diagnostics under `--trace`; `crate::logging::info` writes to log file, not stderr
- **Always** resolve symlinks with `readlink -f` before inspecting binaries with `strings`
- **Ensure `~/.local/bin` is before `~/.cargo/bin` in `PATH`**

## Commands

| Command | Description |
|---------|-------------|
| `jcode run` | Start interactive agent session |
| `jcode self-dev --build` | Build and repoint shared daemon symlink |
| `cargo check -p jcode-tool-core` | Verify core crate compiles |
| `cargo check -p jcode-app-core` | Verify app core compiles |
| `cargo test -p jcode-tool-core` | Run core crate tests |
| `cargo test -p jcode-app-core` | Run app core tests |
| `jcode --version` | Show version info |
| `scripts/install.sh` | Install stable channel |
| `scripts/install_release.sh` | Install immutable release |

## Workflow

1. **Before changing code**: Read `docs/change-impact.md` and `docs/invariants-and-gotchas.md` to understand blast radius and fragile areas
2. **Make code changes** following the app's patterns and conventions
3. **After changing code**: Update affected docs in `/docs/` - especially `change-impact.md`, `invariants-and-gotchas.md`, and `troubleshooting.md`
4. **Verify**: Run `cargo check -p jcode-tool-core && cargo check -p jcode-app-core` to confirm compilation
5. **Test**: Run `cargo test` for the affected crate(s)
6. **Runtime**: Use `jcode self-dev --build` to rebuild the shared daemon, then test with `jcode run`

## Repository Identity
- Use the user's configured `user.name` and `user.email` for commits
- Do not override with `Jcode`, `Jcode agent`, or fabricated emails
- Preserve existing contributor attribution when integrating work

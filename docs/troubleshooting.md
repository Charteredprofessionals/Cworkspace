# Troubleshooting Guide

## Symptom → Likely Cause → Where to Look → Fix

### Build Failures

| Symptom | Likely Cause | Where to Look | Fix |
|---------|-------------|--------------|-----|
| `error[E0509] - cannot find value `child`` | Pre-existing browser.rs error | `crates/jcode-app-core/src/tool/browser.rs` | Check child variable scope; unrelated to require_validator |
| `error[E0535] - [u8] does not implement `Into` `[u8]` | Pre-existing browser.rs error | `crates/jcode-app-core/src/tool/browser.rs` | Check [u8] size conversions; unrelated to require_validator |
| `missing field require_validator` | require_validator field missing from ToolContext | All `ToolContext { ... }` literals | Add `require_validator: false,` to each literal |
| `cargo check -p jcode-app-core` fails | require_validator not added to some literals | Check all ToolContext literals in tool/ directory | Add missing require_validator: false, |
| `cargo check -p jcode-tool-core` fails | ToolContext struct change | `crates/jcode-tool-core/src/lib.rs` | Verify require_validator field and logic |

### Runtime Issues

| Symptom | Likely Cause | Where to Look | Fix |
|---------|-------------|--------------|-----|
| Path validation denies all paths | `require_validator=true` with no validator registered | `verify_path()` in lib.rs:369-377 | Either register a validator or set `require_validator=false` |
| Path validation allows unsafe paths | `require_validator=false` (default) | `verify_path()` in lib.rs:363-377 | Set `require_validator=true` for strict mode |
| `JCODE_COWORK_MODE=1` changes behavior | Cowork mode overrides require_validator | lib.rs:354-358 | Check env var; separate from require_validator flag |
| Sub-calls lose path validation settings | `for_subcall()` not copying require_validator | lib.rs:333-336 | Verify for_suball copies require_validator correctly |

### Test Failures

| Symptom | Likely Cause | Where to Look | Fix |
|---------|-------------|--------------|-----|
| Test compilation fails with require_validator | Test ToolContext literals missing field | `crates/jcode-app-core/src/tool/*_tests.rs` | Add `require_validator: false,` to each test context |
| Path validation behavior changed in tests | require_validator flag change | Test helper functions | Update test contexts, verify expected behavior |

### Environment/Config Issues

| Symptom | Likely Cause | Where to Look | Fix |
|---------|-------------|--------------|-----|
| `~/.local/bin/jcode` points to old binary | Launcher symlink not updated | `~/.local/bin/jcode` → `~/.jcode/builds/current/jcode` | Run `jcode self-dev --build` or reinstall |
| Stable channel not found | `~/.jcode/builds/stable/jcode` missing | `~/.jcode/builds/stable/` | Run `scripts/install.sh` |
| Self-dev build inert | Daemon not restarted after build | `~/.jcode/builds/shared-server/jcode` symlink | Run `jcode self-dev --build` then restart daemon |
| `--trace` shows no output for eprintln! | info! writes to log file, not stderr | Logging system | Use `eprintln!` for throwaway diagnostics |

### How to Read Logs

- **Log file location**: Check `JCODE_LOG` env var or default location
- **Key error messages**:
  - `SECURITY_VIOLATION: no path validator registered for session` - require_validator=true with no validator
  - `SECURITY_VIOLATION: Cowork session has no mounted workspace roots` - JCODE_COWORK_MODE=1
  - `denied_path()` - path denied by validator
- **Log levels**: `info`, `debug`, `trace` (info goes to file, use eprintln! for stderr)

### How to Reproduce Locally

1. **Build check**: `cargo check -p jcode-tool-core && cargo check -p jcode-app-core`
2. **Test path validation**: Create a ToolContext with `require_validator=true`, call `verify_path()` on an unregistered path
3. **Test sub-call**: Use `for_subcall()` and verify `require_validator` is copied
4. **Test browser**: Run `jcode run` and observe browser tool behavior

### How to Roll Back

1. **Git revert**: `git revert <commit-hash>` for the require_validator change
2. **Remove require_validator field**: Delete from ToolContext struct and all references
3. **Re-add old ToolContext literals**: Remove `require_validator: false,` from all literals
4. **Rebuild**: `cargo build -p jcode-tool-core && cargo build -p jcode-app-core`

### Common Environment Mistakes

1. **PATH order**: `~/.local/bin` must be **before** `~/.cargo/bin` - reversed order causes wrong binary to run
2. **Symlink staleness**: `~/.jcode/builds/current/jcode` points to old build after `cargo build`; use `jcode self-dev --build`
3. **Launcher vs binary**: `~/.local/bin/jcode` is a symlink; resolve with `readlink -f` before running `strings`
4. **Canary channel**: `~/.jcode/builds/canary/jcode` exists but is not primary self-dev path; use `self-dev` instead
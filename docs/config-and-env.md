# Config and Environment Variables

## Environment Variables

| Name | Purpose | Where Used | Required? | Breaks If Missing/Wrong |
|------|---------|------------|-----------|------------------------|
| `JCODE_COWORK_MODE` | Enables cowork session mode | `crates/jcode-tool-core/src/lib.rs:354-358` - overrides `require_validator` behavior | No | If set to "1" without workspace roots, `verify_path` raises SECURITY_VIOLATION |
| `JCODE_LOG` | Log output destination | Logging system throughout crates | No | No logs output if not set and stderr not redirected |
| `PATH` | Binary lookup order | Runtime execution | Yes (critical) | **Must have `~/.local/bin` before `~/.cargo/bin`** - reversed order runs wrong binary |
| `HOME` / `~/.jcode` | Jcode data directory location | Channel discovery, builds | No | Falls back to default paths; may miss custom builds |

## Channel Configuration

| Channel | Path | Usage | Install Script |
|---------|------|-------|----------------|
| **Stable** | `~/.jcode/builds/stable/jcode` | Production releases | `scripts/install.sh` |
| **Self-Dev** | `~/.jcode/builds/current/jcode` | Development builds | `jcode self-dev --build` |
| **Canary** | `~/.jcode/builds/canary/jcode` | Testing flows | Not primary path |
| **Immutable Versions** | `~/.jcode/builds/versions/<version>/jcode` | Fixed releases | `scripts/install_release.sh` |

## PATH Configuration

```
Required order: ~/.local/bin BEFORE ~/.cargo/bin
```

**Verification**:
```bash
echo $PATH | tr ':' '\n' | grep -n "local/bin\|cargo/bin"
# ~/.local/bin should appear before ~/.cargo/bin
```

**Waste of time avoiders**:
- `crate::logging::info` writes to log file, not stderr
- `strings` on `builds/shared-server/jcode` reads symlink, not program - use `readlink -f` first

## Feature Flags

Currently no feature flags in cargo.toml. The `JCODE_COWORK_MODE` env var serves as a runtime toggle.

## Configuration Files

- `~/.jcode/builds/current/jcode` - symlink to current build
- `~/.jcode/builds/stable/jcode` - stable release symlink
- `~/.local/bin/jcode` - launcher symlink (must be before cargo bin in PATH)

## Do Not Change Without Reading

1. **`JCODE_COWORK_MODE=1`** - Changes verify_path behavior significantly (lib.rs:354-358)
2. **PATH order** - Critical for which jcode binary runs
3. **Channel symlinks** - Moving builds around breaks launcher
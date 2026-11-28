# Implementation Plan

## Overview

This plan addresses edge case failures in the jcode-Cowork application identified during the security audit. The highest-priority fix is the PathJail bypass in `ToolContext::verify_path()` (`crates/jcode-tool-core/src/lib.rs` lines 357-366), where file paths pass through completely unvalidated when no session path validator is registered and `JCODE_COWORK_MODE` is not set to `"1"`. This means every file tool (`read`, `write`, `edit`, `patch`, `apply_patch`, `replace`, `ls`, `office_docs`, `bash` working_dir, `open`, `side_panel`, `agentgrep`, `compile_remote`) can access arbitrary filesystem paths outside mounted workspace roots in non-Cowork mode. The plan also adds token budget enforcement to `doc_outline` per REQ-DOC-01, adds Linux-compatible tests for Windows-only security vectors, and hardens the `computer_use_win` hotkey registration.

## Types

### PathJailValidationMode (new enum)
File: `crates/jcode-tool-core/src/path_policy.rs`
```rust
pub enum PathJailValidationMode {
    /// PathJail must be active; any path without a registered validator is denied.
    Strict,
    /// PathJail is advisory; unvalidated paths pass through (current default behavior).
    Permissive,
}
```

### ToolContextValidationConfig (new struct)
File: `crates/jcode-tool-core/src/lib.rs`
```rust
pub struct ToolContextValidationConfig {
    pub require_validator: bool, // when true, missing validator = SECURITY_VIOLATION
}
```

## Files

### 1. `crates/jcode-tool-core/src/lib.rs` — MODIFY
- **`ToolContext::verify_path`** (line 357): Change behavior so that when no validator is registered for the session, the path is denied with `SECURITY_VIOLATION` instead of returning `Ok(resolved)` unvalidated. Add a `require_validator` flag to `ToolContext` (default `true` for Cowork sessions, configurable).
- **`ToolContext::resolve_path`** (line 335): Apply the same strict validation — deny when no validator is registered.
- **`ToolContext` struct** (line ~297): Add `require_validator: bool` field.
- **`ToolContext::new`** constructor: Accept `require_validator` parameter or default to `true`.

### 2. `crates/jcode-tool-core/src/path_policy.rs` — MODIFY
- Add `PathJailValidationMode` enum.
- Add `set_validation_mode(session_id, mode)` function to allow per-session mode configuration.
- Add `validation_mode` static map parallel to `VALIDATORS`.

### 3. `crates/jcode-tool-core/src/path_jail.rs` — MODIFY (tests only)
- Add Linux-compatible test for NTFS ADS pattern rejection using `:` in filename component.
- Add test for UNC prefix `\\?\` rejection on all platforms.
- Add test verifying that a path with a symlink inside the workspace pointing outside is rejected at each component level.
- Add test for empty path input.
- Add test for path with trailing separators.
- Add test for path with only `.` components.
- Add test for path with `..` that stays inside root.

### 4. `crates/jcode-app-core/src/context/doc_outline.rs` — MODIFY
- Add `token_budget` parameter to `from_verified_path` (default 2048).
- Add `estimate_tokens(text: &str) -> usize` helper function.
- Truncate output if it exceeds the token budget, appending `[truncated]`.
- Add `#[cfg(test)]` unit tests for token estimation and truncation.

### 5. `crates/jcode-app-core/src/tool/office_docs.rs` — MODIFY
- Update `outline` action call to pass token budget to `from_verified_path`.
- Add test verifying token budget enforcement on a large XLSX outline output.

### 6. `crates/jcode-app-core/src/tool/computer_use_win.rs` — MODIFY
- Change `HOTKEY_LISTENER` from `OnceLock` to lazy initialization at module load.
- Add `reset_panic_switch()` function (internal, for testing).
- Add test verifying `ensure_synthesis_allowed` blocks within <5ms simulation.

### 7. `crates/jcode-tool-core/src/lib.rs` — MODIFY (WinJobGuard)
- Reduce `JOB_MEMORY_LIMIT_BYTES` from 4GB to 512MB.
- Add `#[cfg(test)]` unit test on non-Windows platforms verifying `WinJobGuard::new()` returns `Err(Unsupported)`.

### 8. `crates/jcode-app-core/src/tool/bash.rs` — REVIEW (no code change likely)
- Confirm `WinJobGuard` is dropped when async task is aborted.
- No change needed; the guard is correctly scoped.

## Functions

### New functions
| Name | Signature | File | Purpose |
|------|-----------|------|---------|
| `set_validation_mode` | `fn(session_id: &str, mode: PathJailValidationMode)` | `path_policy.rs` | Configure per-session strict vs permissive validation |
| `estimate_tokens` | `fn(text: &str) -> usize` | `doc_outline.rs` | Rough token count for budget enforcement |
| `truncate_to_budget` | `fn(output: String, budget: usize) -> String` | `doc_outline.rs` | Truncate output to fit within token budget |
| `reset_panic_switch` | `fn() -> Result<(), String>` | `computer_use_win.rs` | Allow re-enabling input after panic (testing/internal) |

### Modified functions
| Name | File | Required Changes |
|------|------|-----------------|
| `ToolContext::verify_path` | `crates/jcode-tool-core/src/lib.rs` | Deny when no validator registered (unless `require_validator=false`) |
| `ToolContext::resolve_path` | `crates/jcode-tool-core/src/lib.rs` | Same strict validation as `verify_path` |
| `from_verified_path` | `crates/jcode-app-core/src/context/doc_outline.rs` | Add `budget: usize` parameter, enforce truncation |
| `ensure_hotkey_listener` | `crates/jcode-app-core/src/tool/computer_use_win.rs` | Call at module load, not just on first `execute()` |

## Classes

### ToolContext (modified)
File: `crates/jcode-tool-core/src/lib.rs`
- Add field: `require_validator: bool`
- Constructor updated to accept `require_validator` parameter

### WinJobGuard (modified)
File: `crates/jcode-tool-core/src/lib.rs` (Windows module)
- Change `JOB_MEMORY_LIMIT_BYTES` from `4 * 1024 * 1024 * 1024` to `512 * 1024 * 1024`

## Dependencies

No new dependencies required. All changes use existing crates:
- `anyhow`, `std::sync::atomic`, `std::collections::HashMap` (already in `path_policy.rs`)
- `serde_json`, `anyhow` (already in `doc_outline.rs`)

## Testing

### Test additions
1. **`crates/jcode-tool-core/src/lib.rs`** — Test that `verify_path` returns error when no validator is registered.
2. **`crates/jcode-tool-core/src/path_jail.rs`** — Linux-compatible tests for ADS (`:`), UNC (`\\?\`), and deep symlink chain rejection.
3. **`crates/jcode-app-core/src/context/doc_outline.rs`** — Token estimation and truncation tests.
4. **`crates/jcode-app-core/src/tool/computer_use_win.rs`** — Panic switch response time test.
5. **`crates/jcode-tool-core/src/lib.rs`** — WinJobGuard unsupported-platform test.

### Validation strategy
- Run `cargo test -p jcode-tool-core` — all tests must pass.
- Run `cargo clippy -p jcode-tool-core --all-targets -- -D warnings` — must be clean.
- Run `cargo fmt --all -- --check` — must pass.
- Run `cargo test -p jcode-app-core --lib doc_outline office_docs computer_use_win` — all tests must pass.

## Implementation Order

1. **Fix PathJail bypass** (`crates/jcode-tool-core/src/lib.rs`): Change `verify_path` and `resolve_path` to deny unvalidated paths. Add `require_validator` field to `ToolContext`.
2. **Add `PathJailValidationMode`** (`crates/jcode-tool-core/src/path_policy.rs`): New enum + per-session mode configuration.
3. **Add PathJail edge case tests** (`crates/jcode-tool-core/src/path_jail.rs`): Linux-compatible ADS, UNC, symlink chain, empty path, trailing separator, dot-only path tests.
4. **Add token budget to doc_outline** (`crates/jcode-app-core/src/context/doc_outline.rs`): `estimate_tokens`, `truncate_to_budget`, budget parameter on `from_verified_path`.
5. **Update office_docs** (`crates/jcode-app-core/src/tool/office_docs.rs`): Pass token budget to `from_verified_path`, add budget test.
6. **Harden computer_use_win** (`crates/jcode-app-core/src/tool/computer_use_win.rs`): Early hotkey registration, panic switch reset, response time test.
7. **Adjust WinJobGuard memory limit** (`crates/jcode-tool-core/src/lib.rs`): Reduce from 4GB to 512MB.
8. **Run full verification**: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test -p jcode-tool-core`, `cargo test -p jcode-app-core --lib doc_outline office_docs computer_use_win`.

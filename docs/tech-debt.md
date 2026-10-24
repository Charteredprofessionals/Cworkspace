# Tech Debt

## Known Technical Debt

### 1. Pre-existing browser.rs Compilation Errors

| Issue | Location | Priority | Fix Complexity |
|-------|----------|----------|----------------|
| 3 "cannot find value `child`" errors | `crates/jcode-app-core/src/tool/browser.rs` | Low (pre-existing, unrelated to require_validator) | Requires understanding child variable scope |
| 3 "[u8]" size errors | `crates/jcode-app-core/src/tool/browser.rs` | Low (pre-existing, unrelated to require_validator) | Type conversion fixes needed |

**Impact**: Browser tool compilation fails; unrelated to the `require_validator` change
**History**: These errors existed before the `require_validator` field was added
**Fix Needed**: Requires code review of browser.rs child variable usage and [u8] conversions

### 2. Incomplete require_validator Field Coverage

| Metric | Value | Impact |
|--------|-------|--------|
| Total `ToolContext { ... }` literals in jcode-app-core | 104 | |
| With `require_validator: false,` | 70 | 67.3% coverage |
| Missing `require_validator: false,` | ~34 | Causes compilation errors |
| Priority | **HIGH** - Missing field causes `missing field require_validator` errors | |

**Impact**: Compilation fails for any code adding new `ToolContext` literals or modifying existing ones
**History**: Added Oct 2026 as part of path validation security enhancement; 70/104 fixed, 34 remain
**Fix Needed**: Add `require_validator: false,` to remaining ~34 literals across:
- `crates/jcode-app-core/src/tool/*.rs`
- `crates/jcode-app-core/src/server/*.rs`
- `crates/jcode-app-core/src/agent/*.rs`
- Test files (`*_tests.rs`)

### 3. Structural Issues Fixed During require_validator Addition

| File | Issue | Fix |
|------|-------|-----|
| `edit_stats.rs` | Struct literal spread operator | Fixed with `..ctx.clone()` pattern |
| `skill.rs` | Struct literal spread operator | Fixed with proper field ordering |
| `todo.rs` | `test_ctx` function | Fixed struct literal |
| `user_response.rs` | `context()` function | Fixed struct literal |

**Impact**: These were fixed as part of the require_validator addition; further changes should verify struct literal syntax
**History**: Fixed during the initial require_validator field addition in Oct 2026

### 4. Daemon Restart Required After Build

| Issue | Location | Impact |
|-------|----------|--------|
| `cargo build` alone proves nothing about behavior | `~/.jcode/builds/shared-server/jcode` daemon | Freshly built binary is inert until daemon restarted |
| `jcode self-dev --build` required | `crates/jcode-tool-core` changes | Runtime checks measure old code without repointing symlink |

**Impact**: Developers may think changes aren't working when they just need to restart the daemon
**Fix**: Run `jcode self-dev --build` after `cargo build -p jcode-tool-core`

### 5. PATH Order Sensitivity

| Issue | Location | Impact |
|-------|----------|--------|
| `~/.local/bin` must be **before** `~/.cargo/bin` in PATH | Shell environment | Running wrong binary if order reversed |
| `strings` on shared-readlink symlink reads 70-byte symlink | `builds/shared-server/jcode` | Must use `readlink -f` first |

**Impact**: Subtle runtime bugs where developer thinks they're testing new code but old code runs
**Fix**: Verify PATH order; use `readlink -f` for binary inspection

### 6. Logging to File, Not Stderr

| Issue | Location | Impact |
|-------|----------|--------|
| `crate::logging::info` writes to log file, not stderr | Throughout codebase | Instrumenting code paths with `info!` produces no output under `--trace` |
| Use `eprintln!` for throwaway diagnostics | | |

**Impact**: Developers may miss visible output when debugging with `--trace`
**Fix**: Use `eprintln!` for diagnostics; delete before committing

## Debt Reduction Progress

| Debt Item | Status | Completed | Remaining |
|-----------|--------|-----------|-----------|
| Add `require_validator: bool` to ToolContext | ✅ Complete | Logic implemented | ~34 literals remaining |
| Fix structural issues in edit_stats.rs, skill.rs, todo.rs, user_response.rs | ✅ Complete | 4 files fixed | 0 remaining |
| Fix browser.rs pre-existing errors | ⚠️ In Progress | 0/6 errors addressed | 6 errors |
| Add `require_validator: false,` to all ToolContext literals | ⚠️ In Progress | 70/104 (67.3%) | ~34/104 (32.7%) |
| Verify `cargo check -p jcode-tool-core` compiles | ✅ Complete | Yes | N/A |

## Recommendations for Future AI Assistants

1. **Always check require_validator coverage** when modifying ToolContext literals
2. **Run `cargo check -p jcode-tool-core`** before and after ToolContext changes
3. **Be aware of browser.rs pre-existing errors** - unrelated but may distract
4. **Remember daemon restart** after `cargo build -p jcode-tool-core`
5. **Use `eprintln!` not `info!`** for throwaway diagnostics under `--trace`
6. **Verify PATH order** has `~/.local/bin` before `~/.cargo/bin`
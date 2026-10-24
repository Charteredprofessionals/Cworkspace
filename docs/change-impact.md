# Change Impact Analysis

## Purpose
Answer "If I change X, what else breaks?" for every major module/feature. This is the most important doc for AI-assisted maintenance.

## Shared Types & Coupling

| Change Area | Blast Radius | Files to Re-verify | Tests to Run |
|-------------|-------------|-------------------|--------------|
| `crates/jcode-tool-core/src/lib.rs` - `ToolContext` struct | **CRITICAL** - All path validation logic depends on this | - `crates/jcode-app-core/src/tool/*.rs` (all ToolContext literals)<br>- `crates/jcode-app-core/src/server/*.rs`<br>- `crates/jcode-agent-runtime/` | `cargo check -p jcode-tool-core`<br>`cargo check -p jcode-app-core` |
| `crates/jcode-tool-core/src/lib.rs` - `require_validator` field | **HIGH** - Path validation on/off switch | - All `ToolContext { ... }` literals need `require_validator: false,`<br>- `crates/jcode-app-core/src/tool/browser.rs`<br>- `crates/jcode-app-core/src/tool/*.tests.rs` | `cargo check -p jcode-tool-core`<br>`cargo check -p jcode-app-core` |
| `crates/jcode-app-core/src/tool/browser.rs` | **HIGH** - Browser tool, 3 pre-existing errors (child scope, [u8] size) | - `crates/jcode-app-core/src/tool/browser_session_tests.rs`<br>- `crates/jcode-app-core/src/tool/browser_tests.rs`<br>- `crates/jcode-app-core/src/tool/browser_fast*.rs` | `cargo check -p jcode-app-core` |
| `crates/jcode-app-core/src/tool/skill.rs` | **MEDIUM** - Skill execution | - `crates/jcode-app-core/src/tool/skill.rs` (struct literal fixes)<br>- Related test files | `cargo check -p jcode-app-core` |
| `crates/jcode-app-core/src/tool/todo.rs` | **MEDIUM** - Todo management | - `crates/jcode-app-core/src/tool/todo.rs` (test_ctx function)<br>- Related test files | `cargo check -p jcode-app-core` |
| `crates/jcode-app-core/src/tool/user_response.rs` | **MEDIUM** - User response flow | - `crates/jcode-app-core/src/tool/user_response.rs` (context() function)<br>- Related test files | `cargo check -p jcode-app-core` |
| `crates/jcode-app-core/src/server/` | **MEDIUM** - Server session mgmt | - All server .rs files<br>- Runtime commands | `cargo check -p jcode-app-core` |
| DB schema changes | **CRITICAL** - Any table/column change | - All repositories using the schema<br>- Migration scripts | Integration tests |
| Feature flags/env vars | **MEDIUM** - New/removed flags | - `config-and-env.md`<br>- All conditional code paths | Feature-specific tests |

## Shared Code Coupling

- `jcode-tool-core` → `jcode-app-core` via `ToolContext` struct
- `jcode-app-core` → `jcode-base` via shared types
- `jcode-app-core` → `jcode-protocol` via message types
- `jcode-agent-runtime` → `jcode-app-core` via agent turn execution

## API Contract Changes

Any change to public functions in:
- `crates/jcode-tool-core/src/lib.rs` → Requires full check of `jcode-app-core`
- `crates/jcode-app-core/src/tool/browser.rs` → Requires browser test review
- `crates/jcode-app-core/src/server/runtime_commands.rs` → Requires server integration tests

## Do Not Change Without Reading

1. **ToolContext struct** (lib.rs:297-322) - Changing fields affects all tool callers
2. **verify_path** function (lib.rs:363-377) - Changes to SECURITY_VIOLATION logic
3. **resolve_path** function (lib.rs:340-362) - Changes to path resolution flow
4. **browser.rs** - Has pre-existing errors; any change may affect browser tests
5. **skill.rs** - Struct literal fixes recently applied; verify before further changes
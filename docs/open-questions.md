# Open Questions

## For Human Confirmation

### 1. require_validator Default Value
**Question**: Should the default value for `require_validator` be `true` (strict mode) or `false` (backward compatible) for new callers?

**Context**: 
- `require_validator: false` was chosen for backward compatibility
- Setting to `true` enables strict path validation enforcement
- All existing callers need `require_validator: false,` added

**Impact**: 
- `true` = stricter security, more compilation errors for new callers
- `false` = backward compatible, fewer immediate errors

**Decision needed**: Confirm the intended default behavior

---

### 2. Browser.rs Pre-existing Errors - Fix or Leave?

**Question**: Should the 6 pre-existing errors in `crates/jcode-app-core/src/tool/browser.rs` (3 "cannot find value `child`" + 3 "[u8]" size errors) be fixed as part of this work, or left as known issues?

**Context**:
- These errors exist independently of the `require_validator` change
- They cause `cargo check -p jcode-app-core` to fail
- Fixing them requires understanding child variable scope and [u8] conversions

**Impact**:
- Fixing: removes compilation errors, improves code quality
- Leaving: documents known issues, but limits full compilation

**Decision needed**: Confirm whether to address browser.rs errors

---

### 3. Remaining 34 ToolContext Literals - Priority Order

**Question**: What's the recommended priority order for adding `require_validator: false,` to the remaining ~34 `ToolContext { ... }` literals?

**Context**:
- 70/104 literals already have the field (67.3% coverage)
- ~34 still need the field
- Files needing fixes include server/, agent/, test/, and tool/ directories

**Impact**:
- Different files have different risk profiles
- Some are test helpers (high impact, easy to fix)
- Some are server/agent code (medium impact)
- Some have structural issues from the original fix

**Decision needed**: Prioritize which files to fix first

---

### 4. JCODE_COWORK_MODE Interaction with require_validator

**Question**: How should `JCODE_COWORK_MODE=1` interact with the `require_validator` flag?

**Context**:
- When `JCODE_COWORK_MODE=1` and `require_validator=false`, `verify_path` still raises SECURITY_VIOLATION if no workspace roots (lib.rs:354-358)
- This is a separate code path from the `require_validator` flag
- The interaction may not be immediately obvious to future maintainers

**Impact**: 
- Future changes to either flag need to understand this interaction
- Documentation in invariants-and-gotchas.md explains this

**Decision needed**: Confirm the intended interaction model

---

### 5. Agent Turn MPSC Streaming Coverage

**Question**: What test coverage is needed for the agent turn MPSC streaming path in `crates/jcode-agent-runtime/`?

**Context**:
- `turn_streaming_mpsc.rs` handles agent turn streaming
- Related to `jcode-app-core` tool execution
- Current test coverage may be insufficient

**Impact**: 
- Insufficient tests may miss regressions in streaming path
- Important for reliability of agent turns

**Decision needed**: Define test coverage goals

---

### 6. Data Model Migration Rules

**Question**: What are the safe migration rules for changing the `ToolContext` struct in future?

**Context**:
- Adding/removing fields ripples through jcode-app-core
- All ToolContext literals must be updated
- `for_subcall()` must copy new fields

**Impact**: 
- Future AI assistants need clear migration guidance
- Currently documented in change-impact.md and invariants-and-gotchas.md

**Decision needed**: Confirm the migration rules are adequate or need expansion
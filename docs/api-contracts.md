# API Contracts

## Public API Surfaces

### jcode-tool-core / ToolContext

| Contract | Description | Impact of Change |
|----------|-------------|-----------------|
| `pub struct ToolContext { ... require_validator: bool }` | Core struct for tool execution context | **CRITICAL** - All jcode-app-core ToolContext literals must be updated |
| `pub fn for_subcall(&self, tool_call_id: String) -> Self` | Creates sub-call with copied context | Copies `require_validator` field - must preserve semantics |
| `pub fn resolve_path(&self, path: &Path) -> PathBuf` | Resolves path with validation | Checks `require_validator` before denying |
| `pub fn verify_path(&self, path: &Path) -> anyhow::Result<PathBuf>` | Validates path against session | Raises SECURITY_VIOLATION if `require_validator=true` and no validator |

### jcode-app-core Tool Dispatch

| Contract | Description | Dependencies |
|----------|-------------|-------------|
| Tool execution API | `ToolContext` → path validation → tool run | Depends on `jcode-tool-core` |
| Browser tool API | Browser session management | Depends on `jcode-tool-core`, has pre-existing errors |
| Skill tool API | Skill execution flow | Depends on `jcode-tool-core` |
| Todo tool API | Todo management | Depends on `jcode-tool-core` |

## Shared Type Contracts

### jcode-session-types

| Type | Used By | Contract |
|------|---------|----------|
| Session management types | `jcode-app-core`, `jcode-agent-runtime` | Must remain compatible across versions |

### jcode-message-types

| Type | Used By | Contract |
|------|---------|----------|
| Message serialization | All crates | Serde-compatible, versioned |

### jcode-protocol

| Type | Used By | Contract |
|------|---------|----------|
| Message protocols | `jcode-app-core`, providers | defines request/response shapes |

## Error Format

### SECURITY_VIOLATION

```
SECURITY_VIOLATION: no path validator registered for session
```
- Origin: `crates/jcode-tool-core/src/lib.rs:371`
- Trigger: `verify_path()` with `require_validator=true` and no registered validator
- Also: `SECURITY_VIOLATION: Cowork session has no mounted workspace roots` when `JCODE_COWORK_MODE=1`

### General anyhow Errors

- Used throughout `jcode-tool-core` and `jcode-app-core`
- Context includes file path and operation being performed

## Consumers of API

### Downstream Crates

| Crate | Consumes | Integration Point |
|-------|----------|-------------------|
| `jcode-app-core` | `ToolContext`, `verify_path`, `resolve_path` | All tool execution |
| `jcode-agent-runtime` | `ToolContext::for_subcall()` | Agent turn dispatch |
| `jcode-desktop-open` | Tool context creation | Desktop integration |
| Provider runtimes | Path validation during tool run | Security boundary |

### Version Compatibility

- `require_validator: false` default maintains backward compatibility
- Adding `require_validator: true` requires updating all ToolContext literals
- Removing the field requires updating `resolve_path()` and `verify_path()` logic
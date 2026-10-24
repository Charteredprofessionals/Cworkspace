# Data Model

## Tables and Entities

### ToolContext

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | `String` | Identifies the session |
| `message_id` | `String` | Identifies the message within session |
| `tool_call_id` | `String` | Identifies the tool call |
| `working_dir` | `Option<PathBuf>` | Working directory for path resolution |
| `stdin_request_tx` | `Option<UnboundedSender<StdinInputRequest>>` | STDIN request channel |
| `graceful_shutdown_signal` | `Option<InterruptSignal>` | Shutdown signal handler |
| `execution_mode` | `ToolExecutionMode` | Agent turn execution mode |
| `require_validator` | `bool` | **New field**: When true, deny paths without registered validator |

**Added**: Oct 2026 as part of path validation security enhancement

### ToolExecutionMode

| Variant | Description |
|---------|-------------|
| `AgentTurn` | Normal agent turn execution |
| `Direct` | Direct execution mode |

### StdinInputRequest

| Field | Type | Description |
|-------|------|-------------|
| (defined in lib.rs:289-296) | | |

### InterruptSignal

| Field | Type | Description |
|-------|------|-------------|
| (defined in lib.rs) | | |

## Relationships

### ToolContext → Session

- `session_id` links to active session
- `message_id` unique within session
- `tool_call_id` scoped to parent call

### ToolContext → Path Validation

- `working_dir` used in `resolve_path_without_validation()` (lib.rs:385-393)
- `require_validator` controls whether `verify_path()` enforces validator check
- When `require_validator=true`: `verify_path()` raises SECURITY_VIOLATION if no validator registered (lib.rs:369-377)
- When `require_validator=false`: `verify_path()` returns resolved path even without validator (lib.rs:379)

### for_subcall → ToolContext

- Copies all fields including `require_validator: self.require_validator` (lib.rs:336)
- Preserves path validation settings across call hierarchy

## Migration Rules

### Adding require_validator Field

1. Add `pub require_validator: bool,` to ToolContext struct (lib.rs:308)
2. Add documentation comment (lib.rs:305-307)
3. Update `for_subcall()` to copy the field (lib.rs:336)
4. Update `resolve_path()` to check `self.require_validator` (lib.rs:344)
5. Update `verify_path()` to check `self.require_validator` (lib.rs:367)
6. Add `require_validator: false,` to all `ToolContext { ... }` literals
7. Run `cargo check -p jcode-tool-core && cargo check -p jcode-app-core`

### Removing require_validator Field

1. Remove `require_validator` field from ToolContext struct
2. Remove all `require_validator: false,` from literals
3. Remove field checks from `resolve_path()` and `verify_path()`
4. Run `cargo check` to verify

## ER Diagram

```mermaid
erDiagram
    TOOL_CONTEXT ||--|{ TOOL_CALL : contains
    TOOL_CONTEXT ||--|{ SESSION : belongs_to
    TOOL_CONTEXT {
        String session_id PK
        String message_id
        String tool_call_id
        Option<PathBuf> working_dir
        bool require_validator
        ToolExecutionMode execution_mode
    }
    SESSION ||--|{ TOOL_CONTEXT : has
    TOOL_EXECUTION_MODE {
        AgentTurn
        Direct
    }
```
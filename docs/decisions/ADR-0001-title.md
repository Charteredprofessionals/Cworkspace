# ADR-0001: Add require_validator Field to ToolContext Struct

## Status
Proposed

## Context
The jcode-tool-core `ToolContext` struct needed a new field to control path validation behavior. The `verify_path` and `resolve_path` functions needed a way to distinguish between callers that require a registered session validator and those that should allow unvalidated paths for backward compatibility.

## Decision
Added `require_validator: bool` field to the `ToolContext` struct in `crates/jcode-tool-core/src/lib.rs` with the following semantics:
- When `true`: `verify_path` and `resolve_path` deny any path that does not have a registered session validator
- When `false` (default for existing callers): unvalidated paths pass through for backward compatibility

Also updated:
- `for_subcall()` method to copy `require_validator: self.require_validator`
- `resolve_path()` to check `self.require_validator` before denying paths
- `verify_path()` to check `self.require_validator` before raising SECURITY_VIOLATION
- ~70/104 `ToolContext { ... }` literals in `jcode-app-core` with `require_validator: false,`

## Alternatives Considered
1. **Don't add the field** - Would require all callers to have path validation, breaking changes
2. **Always deny unvalidated paths** - No backward compatibility option, too restrictive
3. **Add separate struct** - Would fragment the ToolContext type, increase complexity

## Consequences

### Positive
- Backward compatibility maintained (`require_validator: false` default)
- Explicit control over path validation behavior
- Test contexts can opt-in to strict validation

### Negative
- Requires updating ~104 ToolContext literals in jcode-app-core
- ~34 literals still need the field added
- Pre-existing errors in browser.rs unrelated but may distract

### Risk Mitigation
- Default `false` ensures existing code continues to work
- Documentation in invariants-and-gotchas.md and change-impact.md guides future changes
- Verification: `cargo check -p jcode-tool-core` compiles successfully

## Do Not Change Unless
- Requiring strict path validation for all callers (`require_validator: true` default)
- Removing the field entirely and reverting to previous behavior
- Changing the `for_subcall()` copying semantics

## Evidence
- `cargo check -p jcode-tool-core` ✅ Compiles successfully with new field
- 58 files changed, 331 insertions, 91 deletions (git diff --stat)
- Documentation created in: change-impact.md, invariants-and-gotchas.md, troubleshooting.md
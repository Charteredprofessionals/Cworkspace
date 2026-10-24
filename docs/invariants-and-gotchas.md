# Invariants and Gotchas

## Invariants (Must Always Stay True)

1. **Path validation invariants**:
   - `verify_path` with `require_validator=true` and no registered validator → must raise SECURITY_VIOLATION (lib.rs:369-377)
   - `verify_path` with `require_validator=false` and no registered validator → returns resolved path as-is (lib.rs:379)
   - `resolve_path` with `require_validator=true` and `verify_session_path` returns None → must call `denied_path()` (lib.rs:344)

2. **ToolContext field invariant**:
   - `require_validator: bool` must always be present in all `ToolContext { ... }` literals
   - Default value for existing callers is `false` for backward compatibility
   - Adding `require_validator: true` will cause path validation to deny unvalidated paths

3. **Session scoping invariant**:
   - `for_subcall()` copies `require_validator: self.require_validator` (lib.rs:336)
   - Child sub-calls preserve the parent's `require_validator` setting

4. **Browser child scope errors** (pre-existing, unrelated):
   - `crates/jcode-app-core/src/tool/browser.rs` has 3 "cannot find value `child`" errors
   - 3 "[u8]" size errors also in browser.rs
   - These are structural/compilation errors, not logic bugs
   - **Do not fix unless you understand the child scope issue** - these errors existed before the require_validator change

## Gotchas (Non-obvious Behavior)

1. **require_validator default is false**:
   - New code should add `require_validator: false,` for backward compatibility
   - Setting to `true` enables strict path validation for new callers
   - Existing callers without the field will fail to compile

2. **Spread operator gotcha in ToolContext literals**:
   ```rust
   // WRONG: require_validator after .. spread
   ToolContext {
       working_dir: ...,
       require_validator: false,  // error: cannot be after .. 
       ..other_ctx,
   }
   
   // CORRECT: require_validator before .. spread  
   ToolContext {
       working_dir: ...,
       require_validator: false,
       ..other_ctx,  // OK: spread is last
   }
   ```
   
   // ALSO CORRECT (field before ..):
   ToolContext {
       working_dir: ...,
       ..other_ctx,
       require_validator: false,  // OK: field after spread if it's a new field
   }
   ```

3. **for_subcall preserves require_validator**:
   - When creating a sub-call, the `require_validator` field is copied from parent
   - This ensures path validation consistency across call hierarchies

4. **JCODE_COWORK_MODE environment variable**:
   - When set to "1", `verify_path` with `require_validator=false` and no validator → still raises SECURITY_VIOLATION (lib.rs:354-358)
   - This is a separate code path from the `require_validator` flag

5. **Test context literals must be updated**:
   - Any `ToolContext { ... }` in test files must include `require_validator: false,`
   - Failure to do so causes compilation errors
   - See: `crates/jcode-app-core/src/tool/*_tests.rs` files

## Fragile Areas ("Do Not Touch Without Reading X")

1. **`crates/jcode-app-core/src/tool/browser.rs`** - Pre-existing compilation errors; any change needs understanding of child scope and [u8] size issues
2. **`crates/jcode-tool-core/src/lib.rs` ToolContext struct** - Adding/removing fields ripples through jcode-app-core
3. **`crates/jcode-app-core/src/tool/skill.rs`** - Recently fixed struct literals; verify before modifying
4. **`crates/jcode-app-core/src/tool/todo.rs`** - test_ctx function recently fixed; verify changes
5. **All `ToolContext { ... }` literals** - Must include `require_validator: false,` or compilation fails

## Past Bugs and Fixes

- **require_validator field addition** (Oct 2026): Added `require_validator: bool` to ToolContext, updated 70/104 literals in jcode-app-core, 34 remain
- **Structural fixes in edit_stats.rs, skill.rs, todo.rs, user_response.rs**: Fixed struct literal spread operator issues when adding require_validator field
- **browser.rs pre-existing errors**: 3 "cannot find value `child`" and 3 "[u8]" size errors exist independently of require_validator change
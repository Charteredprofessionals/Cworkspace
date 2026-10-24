# Testing Guide

## Test Organization

### Crate-Level Tests

| Crate | Test File | Description |
|-------|-----------|-------------|
| `jcode-tool-core` | Unit tests in `lib.rs` | ToolContext, path validation logic |
| `jcode-app-core` | `tool/*_tests.rs` | Tool-specific tests (browser, skill, todo, etc.) |
| `jcode-app-core` | `tests.rs` | Comprehensive test suite |

### Test Types

| Test Type | Location | Scope |
|-----------|----------|-------|
| Unit tests | `lib.rs` functions | ToolContext, path validation |
| Integration tests | `tool/*_tests.rs` | Individual tool behavior |
| End-to-end | `jcode run` sessions | Full agent workflow |

## What Is and Isn't Covered

### Covered

- `cargo check -p jcode-tool-core` - compiles core crate
- `cargo check -p jcode-app-core` - compiles app core (partial, some missing require_validator)
- Path validation logic: `verify_path()`, `resolve_path()`
- `for_subcall()` field copying behavior

### Not Covered (gaps)

- Full end-to-end agent workflow tests
- Browser tool tests (has pre-existing compilation errors in browser.rs)
- All `ToolContext { ... }` literal coverage - ~34 missing `require_validator: false,`
- Server runtime command tests
- Agent turn MPSC streaming tests

## How to Verify a Change Safely

### 1. Build Check

```bash
cargo check -p jcode-tool-core
cargo check -p jcode-app-core
```

### 2. Specific Tests

```bash
# Run jcode-tool-core tests
cargo test -p jcode-tool-core

# Run specific test modules
cargo test -p jcode-app-core -- tool::skill
cargo test -p jcode-app-core -- tool::todo
```

### 3. Path Validation Tests

Test `verify_path` and `resolve_path` with different `require_validator` settings:

```rust
// Test with require_validator=true
let ctx = ToolContext {
    require_validator: true,
    // ... other fields
};
let result = ctx.verify_path(&path);
// Should Err with SECURITY_VIOLATION if no validator registered

// Test with require_validator=false
let ctx = ToolContext {
    require_validator: false,
    // ... other fields
};
let result = ctx.verify_path(&path);
// Should Ok(resolved path) even without validator
```

### 4. Sub-call Tests

```rust
let parent = ToolContext {
    require_validator: true,
    // ... other fields
};
let child = parent.for_subcall("new_call_id".to_string());
// child.require_validator should be true (copied from parent)
```

## How Tests Guard Which Areas

| Area | Guarding Tests | Risk if Broken |
|------|---------------|----------------|
| Path validation | `verify_path`, `resolve_path` in lib.rs | Security bypass or path denial |
| ToolContext struct | All ToolContext literals compilation | All tool calls fail to compile |
| Sub-call behavior | `for_subcall()` field copying | Child calls have wrong validation settings |
| Browser tool | `crates/jcode-app-core/src/tool/browser.rs` | Browser tests fail (pre-existing errors) |

## Running Tests

```bash
# Full test suite (may have some failures due to pre-existing issues)
cargo test -p jcode-tool-core
cargo test -p jcode-app-core  # May have some skips/failures

# Specific tool tests
cargo test -p jcode-app-core -- skill
cargo test -p jcode-app-core -- todo
cargo test -p jcode-app-core -- user_response

# Check compilation only (no tests)
cargo check -p jcode-tool-core
cargo check -p jcode-app-core
```
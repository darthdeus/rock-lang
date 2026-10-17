# Formatter Test Suite

This directory contains comprehensive tests for the Rock formatter.

## Test File Types

### `.fmt` Files
Before/after format tests using test cases:

```
//=== TEST CASE: test_case_name ===
input code here

//=== EXPECTED ===
expected output here

//=== TEST CASE: another_test_case ===
more input code

//=== EXPECTED ===
more expected output
```

Each test case runs independently and is reported separately in the results.

#### No Change Tests
For test cases where the formatter should leave the code unchanged, use `//=== NO CHANGE ===` instead of `//=== EXPECTED ===`. This avoids duplicating identical code:

```
//=== TEST CASE: already_formatted ===
fn perfectly_formatted() { return 42 }

//=== NO CHANGE ===
```

### `.idempotent` Files  
Files that should remain completely unchanged when formatted. Used for testing that well-formatted code stays well-formatted.

### `.should_fail` Files
Files that should fail to format due to syntax errors. Used to ensure the formatter handles errors gracefully.

## Running Tests

```bash
# Run all formatter tests
just fmt-test

# Run specific test category
just fmt-test attributes
just fmt-test comments

# Check specific file
cargo run --bin rock fmt --check tests/fmt/comments/aligned_inline.rock.fmt
```

## Adding New Tests

1. Choose the appropriate category directory
2. Create a new test file with descriptive name
3. Add test metadata as comments at the top:
   - `//! Test: Brief description`
   - `//! Tags: tag1, tag2` (optional)
   - `//! TODO: Fix this` (for known issues)

### Example Multi-Test Case File

```rock
//! Test: Multiple inline function test cases
//! Tags: inline, functions, multiple

//=== TEST CASE: already_perfect ===
inline(lua) fn simple() -> int = "return 42"

//=== NO CHANGE ===

//=== TEST CASE: needs_indentation ===
inline(lua) fn complex(x: int) -> int = """
local result = x * 2
return result
"""

//=== EXPECTED ===
inline(lua) fn complex(x: int) -> int = """
    local result = x * 2
    return result
"""

//=== TEST CASE: function_collapse ===
fn multiline_func() {
    return 42
}

//=== EXPECTED ===
fn multiline_func() { return 42 }
```

This example shows:
- A test case that expects no changes (`NO CHANGE`)
- A test case that expects formatting changes (`EXPECTED`)  
- Mixed formatting scenarios in a single file

## Test Categories

- **attributes/** - Decorator and attribute formatting
- **control_flow/** - If, match, loop formatting  
- **comments/** - Comment preservation and alignment
- **expressions/** - Expression formatting, function calls, operators
- **real_world/** - Real code from stdlib and games
- **regressions/** - Specific bug fixes to prevent regressions
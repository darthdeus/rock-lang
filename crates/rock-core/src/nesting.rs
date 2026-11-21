//! The parser depth limit and compiler thread stack size must stay aligned.

/// Nested expressions and blocks each count as a level; a nested `if` costs two.
#[cfg(not(target_family = "wasm"))]
pub const MAX_NESTING_DEPTH: u32 = 8192;

/// Wasm calls use the host's native stack, which the module cannot size.
#[cfg(target_family = "wasm")]
pub const MAX_NESTING_DEPTH: u32 = 512;

/// Reserved thread stack for recursive passes at the nesting limit.
pub const COMPILER_STACK_BYTES: usize = 256 << 20;

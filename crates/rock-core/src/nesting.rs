//! How deeply source may nest, and the thread stack that compiling such
//! source needs. The two numbers are one contract: the parser rejects deeper
//! input with a diagnostic, and every thread that runs compiler passes gets a
//! stack large enough for the recursive passes at that depth.

/// Expression and block levels the parser accepts. A level is an expression
/// nested in another (an operand, argument, condition or parenthesized
/// expression, including each operand of a left-associative `a + b + c`
/// chain) or a block; a nested `if` costs two levels.
#[cfg(not(target_family = "wasm"))]
pub const MAX_NESTING_DEPTH: u32 = 8192;

/// On wasm the binding stack is the engine's, which the module cannot size:
/// wasm calls run on the host's native stack (about 984 KiB in V8), beside
/// the 8 MiB linear-memory stack set in `.cargo/config.toml`. Under node 24
/// nested calls `f(f(..))` trap past 655 levels and nested `if`s past 786,
/// and the linear stack holds 841 `+` operands. A call or an `if` costs two
/// levels, so 512 uses at most about 60% of either stack, leaving headroom
/// for engines with smaller ones.
#[cfg(target_family = "wasm")]
pub const MAX_NESTING_DEPTH: u32 = 512;

/// Stack size for threads that run compiler passes. In a release build a
/// `+` chain, the costliest construct measured, takes about 11 KiB per level
/// across the recursive passes, so `MAX_NESTING_DEPTH` levels need about
/// 90 MiB; the rest is headroom for constructs not measured. It is reserved
/// address space: a thread commits pages only as deep source touches them.
pub const COMPILER_STACK_BYTES: usize = 256 << 20;

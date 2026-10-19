//! How deeply source may nest, and the thread stack that compiling such
//! source needs. The two numbers are one contract: the parser rejects deeper
//! input with a diagnostic, and every thread that runs compiler passes gets a
//! stack large enough for the recursive passes at that depth.

/// Expression and block levels the parser accepts. A level is an expression
/// nested in another (an operand, argument, condition or parenthesized
/// expression, including each operand of a left-associative `a + b + c`
/// chain) or a block; a nested `if` costs two levels.
pub const MAX_NESTING_DEPTH: u32 = 8192;

/// Stack size for threads that run compiler passes. In a release build a
/// `+` chain, the costliest construct measured, takes about 11 KiB per level
/// across the recursive passes, so `MAX_NESTING_DEPTH` levels need about
/// 90 MiB; the rest is headroom for constructs not measured. It is reserved
/// address space: a thread commits pages only as deep source touches them.
pub const COMPILER_STACK_BYTES: usize = 256 << 20;

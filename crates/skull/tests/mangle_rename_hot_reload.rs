use crate::common::{emit_without_stdlib, run_luajit};

const V1: &str = "\
fn foo(x: int) -> int { return 51 }
fn probe() -> int { return foo(5) }
fn main() {}
";

const V2_OVERLOAD: &str = "\
fn foo(x: int) -> int { return 52 }
fn foo(s: string) -> string { return s }
fn probe() -> int { return foo(5) }
fn main() {}
";

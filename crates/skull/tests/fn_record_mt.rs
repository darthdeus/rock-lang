use crate::common::{emit_module, run_luajit};

// Called twice so the lambda stays a code slot instead of being spliced.
const LAMBDA: &str = "fn main() { let f = () => 1; print(f() + f()) }\n";

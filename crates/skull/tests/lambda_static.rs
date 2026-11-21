use crate::common::{emit_module, run_luajit};

const PROBE: &str = r#"
global RESULT: int = 0
global THUNK: (() -> int)? = null

fn setup() {
    THUNK = () => {
        static n = 100
        n = n + 1
        return n
    }
}

fn call() -> int {
    let f = THUNK
    if f != null { return f() }
    return -1
}

fn main() { setup() RESULT = call() }
"#;

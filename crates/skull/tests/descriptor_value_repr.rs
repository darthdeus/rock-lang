use crate::common::{emit_v1_package, mangled_name, run_luajit};

const USER_NAMED: &str = r#"
value NativeBuffer { a: int, b: float }

fn take(p: NativeBuffer) -> float {
    return p.b + (p.a as float)
}

pub fn probe() -> float {
    let p = NativeBuffer { a: 1, b: 2.0 }
    let q = p
    return take(q)
}
"#;

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

#[test]
fn user_value_named_like_a_descriptor_keeps_scalarization() {
    let emitted = emit_v1_package("userdesc", &[("main.rock", USER_NAMED)]);
    let take = mangled_name(&emitted.lua, "take");
    let header = emitted
        .lua
        .lines()
        .find(|l| l.starts_with(&format!("function {take}(")))
        .expect("take is emitted");
    assert!(header.contains(", "));
    let probe = mangled_name(&emitted.lua, "probe");
    assert_eq!(
        run_luajit(&format!("{}\nprint({probe}())\n", emitted.lua)),
        "3"
    );
}

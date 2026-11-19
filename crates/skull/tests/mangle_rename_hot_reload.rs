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

#[test]
fn overload_add_orphans_running_global_sentinel() {
    let before = emit_without_stdlib(V1);
    let after = emit_without_stdlib(V2_OVERLOAD);
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         _G.HOT_RELOAD = true\n{after}\n\
         print('fresh=' .. probe_0())\n\
         print('canonical=' .. foo_int(5))\n\
         print('bare_foo_present=' .. tostring(_G.foo ~= nil))\n"
    );
    assert_eq!(
        run_luajit(&script),
        "fresh=52\ncanonical=52\nbare_foo_present=false"
    );
}

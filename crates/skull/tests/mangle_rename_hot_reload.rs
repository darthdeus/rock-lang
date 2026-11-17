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
    for lua in [&before, &after] {
        assert!(
            lua.contains("\nfoo_int = function(")
                || lua.contains("\nfunction foo_int(")
                || lua.contains("__rock_mk_foo_int")
        );
    }
    let script = format!(
        "_G.HOT_RELOAD = false\n{before}\n\
         _G.HOT_RELOAD = true\n{after}\n\
         print('fresh=' .. probe_0())\n\
         print('canonical=' .. foo_int(5))\n\
         print('bare_foo_present=' .. tostring(_G.foo ~= nil))\n"
    );
    let out = run_luajit(&script);
    let fresh = out.lines().find(|l| l.starts_with("fresh=")).unwrap();
    let canon = out.lines().find(|l| l.starts_with("canonical=")).unwrap();
    let bare = out
        .lines()
        .find(|l| l.starts_with("bare_foo_present="))
        .unwrap();

    assert_eq!(fresh, "fresh=52");
    // The reloaded chunk's authoritative int fn carries the edit, at the SAME name.
    assert_eq!(canon, "canonical=52");
    // No bare `foo` global exists under always-mangle -- there is nothing to orphan.
    assert_eq!(bare, "bare_foo_present=false");
}

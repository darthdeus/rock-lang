use crate::common::{emit_module, mangled_name, run_luajit};

#[test]
fn record_fields_hold_no_inline_closures() {
    let src = r#"
fn mk_snap(x: int) -> fn() -> int { return () => x }
fn main() { let _ = mk_snap(1) }
"#;
    let lua = emit_module(src);
    let mk = mangled_name(&lua, "mk_snap");
    let driver = format!(
        "\nlocal a = {mk}(1)\nlocal b = {mk}(2)\n\
        print(rawequal(a.__slot, b.__slot))\n\
        local extra = false\n\
        for k, v in pairs(a) do if k ~= \"__slot\" and type(v) == \"function\" then extra = true end end\n\
        print(extra)\n"
    );
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "true\nfalse");
}

#[test]
fn deepcopy_record_callback_stays_callable() {
    let src = r#"
struct Holder { cb: fn() -> int }
@nomangle inline(lua) fn stash(h: Holder) = "_G.__H2 = h"
fn main() {
    let n = 41
    let h = Holder { cb: () => n + 1 }
    stash(deepcopy(h))
}
"#;
    let lua = emit_module(src);
    let driver = "\n\
        main()\n\
        local ok, res = pcall(function() return __H2.cb() end)\n\
        print(tostring(ok) .. \"\\t\" .. tostring(res):gsub(\":.*\", \"\"))\n";
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "true\t42");
}

#[test]
fn deepcopy_record_boxed_capture_diverges_from_original() {
    let src = r#"
struct Pair { get: fn() -> int, bump: fn() -> int }
@nomangle inline(lua) fn stash_pair(p: Pair) = "_G.__P = p"
fn make_pair() -> Pair {
    let n = 10
    return Pair { get: () => n, bump: () => { n = n + 1 return n } }
}
fn main() {
    let orig = make_pair()
    stash_pair(deepcopy(orig))
    let _ = orig.bump()
    let _ = orig.bump()
    stash_orig(orig)
}
@nomangle inline(lua) fn stash_orig(p: Pair) = "_G.__O = p"
"#;
    let lua = emit_module(src);
    let driver = "\n\
        main()\n\
        print(tostring(__O.get()) .. \"\\t\" .. tostring(__P.get()))\n";
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "12\t10");
}

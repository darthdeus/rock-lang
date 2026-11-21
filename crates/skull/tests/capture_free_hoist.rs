use crate::common::{emit_module, function_body, mangled_name, run_luajit};

#[test]
fn defer_retains_an_existing_cleanup_without_a_forwarding_capture() {
    let lua = emit_module(
        r#"
struct Owner { borrowed: bool = false, release: (fn())? = null }
fn owner_new() -> Owner {
    let owner = Owner {}
    owner.release = () => { owner.borrowed = false }
    return owner
}
fn borrow(owner: Owner, fail: bool, other: Owner? = null) {
    assert(!owner.borrowed)
    owner.borrowed = true
    let cleanup = owner.release.unwrap()
    defer { cleanup() }
    if let next = other { borrow(next, fail) }
    if fail { error("body failure") }
}
fn main() { let _ = owner_new() }
"#,
    );
    let body = function_body(&lua, "borrow");
    assert!(!body.contains("setmetatable("));
    let make = mangled_name(&lua, "owner_new");
    let borrow = mangled_name(&lua, "borrow");
    let driver = format!(
        r#"
local a, b = {make}(), {make}()
{borrow}(a, false, nil)
assert(not a.borrowed)
a.borrowed = true
{borrow}(b, false, nil)
assert(a.borrowed and not b.borrowed)
a.borrowed = false
local ok, err = pcall({borrow}, a, true, b)
assert(not ok and tostring(err):find("body failure", 1, true))
assert(not a.borrowed and not b.borrowed)
assert(__rock_defer_top == 0)
print("released")
"#
    );
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "released");
}

#[test]
fn defer_keeps_late_binding_for_reassigned_cleanup() {
    let lua = emit_module(
        r#"
inline(lua) fn first() = "print('first')"
inline(lua) fn second() = "print('second')"
fn choose() {
    let cleanup = first
    defer { cleanup() }
    cleanup = second
    error("fail")
}
fn main() {}
"#,
    );
    let choose = mangled_name(&lua, "choose");
    assert_eq!(run_luajit(&format!("{lua}\npcall({choose})\n")), "second");
}

#[test]
fn hoisted_body_declares_own_cells_no_global_leak() {
    let src = r#"
fn split_probe() -> fn() -> int {
    return () => {
        let a = 1
        let b = 2
        let c = a + b
        return c
    }
}
fn main() { let f = split_probe() print("{f()}") }
"#;
    let lua = emit_module(src);
    let driver = "\nmain()\n\
        local leaked = {}\n\
        for k in pairs(_G) do if type(k) == \"string\" and k:match(\"^c%d+$\") then leaked[#leaked + 1] = k end end\n\
        table.sort(leaked)\n\
        print(#leaked == 0 and \"no-leak\" or table.concat(leaked, \",\"))\n";
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "3\nno-leak");
}

use crate::common::{emit_module, function_body, mangled_name, run_luajit, run_main};

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

const FIXTURE: &str = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"

fn free_loop() -> [fn()] {
    let fns: [fn()] = []
    for i in 0..3 {
        fns.push(() => { sink(42) })
    }
    return fns
}

fn capturing_loop() -> [fn() -> int] {
    let fns: [fn() -> int] = []
    for i in 0..3 {
        let x = i
        fns.push(() => x)
    }
    return fns
}

fn main() { let _ = free_loop() let _ = capturing_loop() }
"#;

#[test]
fn capture_free_lambda_hoists_to_named_toplevel() {
    let lua = emit_module(FIXTURE);
    let lam = format!("{}__lambda0", mangled_name(&lua, "free_loop"));
    assert!(lua.contains(&format!("function {lam}(")));
    let body = function_body(&lua, "free_loop");
    assert!(body.contains("__rock_code_slot_ref("));
    // Only an anonymous closure renders `function(` without a space.
    assert!(!body.contains("function("));
}

#[test]
fn hoisted_lambda_shared_across_creations() {
    let src = r#"
@nomangle inline(lua) fn sink(a: int) = "_G.__sink = a"
fn mk() -> fn() { return () => { sink(1) } }
fn main() { let _ = mk() }
"#;
    let lua = emit_module(src);
    let mk = mangled_name(&lua, "mk");
    let driver = format!("\nlocal a = {mk}()\nlocal b = {mk}()\nprint(rawequal(a, b))\n");
    assert_eq!(run_luajit(&format!("{lua}{driver}")), "true");
}

#[test]
fn transitive_root_capture_hoists_both() {
    let src = r#"
fn transitive(x: int) -> fn() -> int {
    let outer = () => { return () => x }
    return outer()
}
fn main() { let g = transitive(7) print("{g()}") }
"#;
    let lua = emit_module(src);
    let m = mangled_name(&lua, "transitive");
    assert!(
        lua.contains(&format!("function {m}__lambda0(c"))
            && lua.contains(&format!("function {m}__lambda1(c"))
    );
    assert_eq!(run_main(&lua), "7");
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

use crate::common::emit_module;

const SRC: &str = r#"
newtype Handle(int)
newtype Ratio(float)
newtype Flag(bool)
newtype Alias(Handle)
newtype Boxed<T>(T)
newtype Pair<T>(T)
newtype Deep(Pair<Pair<int>>)
newtype Wrap<T>(Boxed<T>)
newtype Outer<T>(Pair<Pair<T>>)
newtype Twice<A, B>(Pair<B>)

extern(ffi, "m") fn ret_int() -> int
extern(ffi, "m") fn ret_handle() -> Handle
extern(ffi, "m") fn ret_alias() -> Alias
extern(ffi, "m") fn ret_boxed() -> Boxed<int>
extern(ffi, "m") fn ret_deep() -> Deep
extern(ffi, "m") fn ret_wrap() -> Wrap<int>
extern(ffi, "m") fn ret_outer() -> Outer<int>
extern(ffi, "m") fn ret_twice() -> Twice<float, int>
extern(ffi, "m") fn ret_outer_float() -> Outer<float>
extern(ffi, "m") fn ret_ratio() -> Ratio
extern(ffi, "m") fn ret_boxed_float() -> Boxed<float>
extern(ffi, "m") fn ret_flag() -> Flag
extern(ffi, "m") fn ret_bool() -> bool
extern(ffi, "m") fn ret_void()

fn main() {
    print("{ret_int()} {ret_handle().0} {ret_alias().0.0} {ret_ratio().0} {ret_flag().0}")
    ret_void()
}
"#;

fn wrapper(lua: &str, name: &str) -> String {
    let body = lua
        .lines()
        .skip_while(|l| !l.starts_with(&format!("function {name}(")))
        .take_while(|l| *l != "end")
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.is_empty());
    body
}

fn reject(src: &str) -> String {
    let err = crate::common::try_emit_module(src)
        .expect_err("this return has no C representation to give");
    format!("{err:?}")
}

#[test]
fn a_newtype_return_cannot_smuggle_a_gc_value_across_extern() {
    let err = reject(
        r#"
value Bag { items: [int] }
newtype Wrap<T>(T)
extern(ffi, "m") fn ret_bag() -> Wrap<Wrap<Bag>>
fn main() { print("x") }
"#,
    );
    assert!(err.contains("Bag") && err.contains("GC-carrying `value` struct"));

    let ffi_clean = crate::common::try_emit_module(
        r#"
value Point { x: int, y: float }
newtype Wrap<T>(T)
extern(ffi, "m") fn ret_point() -> Wrap<Wrap<Point>>
fn main() { print("x") }
"#,
    );
    assert!(ffi_clean.is_ok());
}

#[test]
fn an_int_representation_is_coerced_through_every_alias_shape() {
    let lua = emit_module(SRC);

    for name in [
        "ret_int",
        "ret_handle",
        "ret_alias",
        "ret_boxed",
        "ret_deep",
        "ret_wrap",
        "ret_outer",
        "ret_twice",
    ] {
        let w = wrapper(&lua, name);
        assert!(
            w.contains(&format!("local __rock_ret = tonumber(m.{name}("))
                && w.contains("return __rock_ret")
        );
    }

    for name in ["ret_ratio", "ret_boxed_float", "ret_outer_float"] {
        let w = wrapper(&lua, name);
        assert!(w.contains(&format!("local __rock_ret = m.{name}(")) && !w.contains("tonumber"));
    }

    let w = wrapper(&lua, "ret_void");
    assert!(!w.contains("return"));
}

#[test]
fn a_bool_representation_is_compared_to_the_byte_the_cdef_returns() {
    let lua = emit_module(SRC);

    for name in ["ret_bool", "ret_flag"] {
        let w = wrapper(&lua, name);
        assert!(w.contains(&format!("return m.{name}(")) && w.trim_end().ends_with("~= 0"));
        assert!(!w.contains("tonumber"));
    }
}

#[test]
fn an_unbound_generic_representation_rejects() {
    let msg = reject(
        r#"
newtype Boxed<T>(T)
extern(ffi, "m") fn ret_open<T>() -> Boxed<T>
fn main() { print("x") }
"#,
    );
    assert!(msg.contains("ret_open") && msg.contains("unbound generic parameter"));
}

#[test]
fn a_self_referential_alias_rejects_at_definition() {
    let errs = crate::common::bone_rejections(
        r#"
newtype A(B)
newtype B(A)
extern(ffi, "m") fn ret_cycle() -> A
fn main() { print("x") }
"#,
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("recursive value type: A -> B -> A"))
    );
}

#[test]
fn a_generic_alias_cycle_rejects_at_definition() {
    let errs = crate::common::bone_rejections(
        r#"
newtype X<T>(Y<T>)
newtype Y<U>(X<U>)
extern(ffi, "m") fn ret_gcycle() -> X<int>
fn main() { print("x") }
"#,
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("recursive value type: X -> Y -> X"))
    );
}

#[test]
fn every_value_returning_wrapper_returns_through_a_local() {
    let lua = emit_module(SRC);
    for name in [
        "ret_int",
        "ret_handle",
        "ret_alias",
        "ret_boxed",
        "ret_deep",
        "ret_wrap",
        "ret_outer",
        "ret_twice",
        "ret_ratio",
        "ret_boxed_float",
        "ret_outer_float",
    ] {
        let w = wrapper(&lua, name);
        let tail = w.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("return m.")
                || t.starts_with("return tonumber(")
                || t.starts_with("return ffi.new(")
        });
        assert!(!tail);
        assert!(w.contains("return __rock_ret"));
    }
}

#[test]
fn an_omitted_extern_literal_default_is_passed_and_filled_in_the_wrapper() {
    let lua = emit_module(
        r#"
extern(ffi, "m") fn poll(id: int, wait: bool = false) -> int
fn go() -> int { poll(-1) }
"#,
    );
    let w = wrapper(&lua, "poll");
    assert!(w.contains("if wait == nil then wait = false end"));
    let go = crate::common::function_body(&lua, "go");
    assert!(go.contains("poll((-1), (false))"));
}

#[test]
fn a_v1_extern_wrapper_uses_the_declared_symbol() {
    let emitted = crate::common::emit_v1_package(
        "v1extern",
        &[(
            "main.rock",
            r#"
extern(ffi, "m") fn ret_int() -> int
extern(ffi, "m") fn ret_bool() -> bool
pub fn probe() -> string { return "{ret_int()} {ret_bool()}" }
fn main() {}
"#,
        )],
    );
    assert_eq!(
        crate::common::run_luajit(&format!(
            "m = {{ ret_int = function() return 7 end, ret_bool = function() return 1 end }}\n{}\nprint(ret_int(), ret_bool())\n",
            emitted.lua
        )),
        "7\ttrue"
    );
}

#[test]
fn a_v1_extern_value_return_allocates_by_the_type_symbol() {
    let emitted = crate::common::emit_v1_package(
        "v1extval",
        &[(
            "main.rock",
            r#"
pub value Pair { a: float, b: float }
extern(ffi, "m") fn get_pair() -> Pair
pub @nomangle fn probe() -> string {
    let p = get_pair()
    return "{p.a + p.b}"
}
fn main() {}
"#,
        )],
    );
    let out = crate::common::run_luajit(&format!(
        "m = {{ get_pair = function(out) out[0].a = 3; out[0].b = 4 end }}\n{}\nprint(probe())\n",
        emitted.lua
    ));
    assert_eq!(out, "7");
}

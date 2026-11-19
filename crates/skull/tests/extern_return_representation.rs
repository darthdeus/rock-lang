use crate::common::{
    bone_rejections, emit_module, emit_v1_package, function_body, run_luajit, try_emit_module,
};

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
    let err = try_emit_module(src).expect_err("this return has no C representation to give");
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

    let ffi_clean = try_emit_module(
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
        assert!(w.contains("return __rock_ret"));
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
fn an_alias_cycle_rejects_at_definition() {
    for (src, cycle) in [
        (
            "newtype A(B)\nnewtype B(A)\nextern(ffi, \"m\") fn ret() -> A",
            "A -> B -> A",
        ),
        (
            "newtype X<T>(Y<T>)\nnewtype Y<U>(X<U>)\nextern(ffi, \"m\") fn ret() -> X<int>",
            "X -> Y -> X",
        ),
    ] {
        let errs = bone_rejections(&format!("{src}\nfn main() {{ print(\"x\") }}\n"));
        assert!(
            errs.iter()
                .any(|e| e.contains(&format!("recursive value type: {cycle}"))),
            "{errs:?}"
        );
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
    let go = function_body(&lua, "go");
    assert!(go.contains("poll((-1), (false))"));
}

#[test]
fn a_v1_extern_wrapper_uses_the_declared_symbol() {
    let emitted = emit_v1_package(
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
        run_luajit(&format!(
            "m = {{ ret_int = function() return 7 end, ret_bool = function() return 1 end }}\n{}\nprint(ret_int(), ret_bool())\n",
            emitted.lua
        )),
        "7\ttrue"
    );
}

#[test]
fn a_v1_extern_value_return_allocates_by_the_type_symbol() {
    let emitted = emit_v1_package(
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
    let out = run_luajit(&format!(
        "m = {{ get_pair = function(out) out[0].a = 3; out[0].b = 4 end }}\n{}\nprint(probe())\n",
        emitted.lua
    ));
    assert_eq!(out, "7");
}

use crate::common::{emit_module, function_body, run_luajit};

#[test]
fn freshly_allocated_buffer_fill_uses_constructor_length() {
    let lua = emit_module(
        r#"
@nomangle fn fill_buffer(count: int) -> int {
    let buf = native_buffer_int(count)
    for i in 0..count { buf[i] = i }
    return buf.len
}
"#,
    );
    let body = function_body(&lua, "fill_buffer");
    assert!(!body.contains("__nb_checked"), "{body}");
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nassert(fill_buffer(0) == 0)\nassert(fill_buffer(17) == 17)\nprint('ok')"
        ))
        .trim(),
        "ok"
    );
}

#[test]
fn fresh_byte_buffer_fill_uses_constructor_length() {
    let lua = emit_module(
        r#"
@nomangle fn fill_bytes(count: int) -> int {
    let buf = native_buffer_u8(count)
    for i in 0..count { buf[i] = i }
    return buf.len
}
"#,
    );
    let body = function_body(&lua, "fill_bytes");
    assert!(!body.contains("__nb_checked"), "{body}");
}

#[test]
fn constructor_bounds_survive_local_aliases() {
    let lua = emit_module(
        r#"
@nomangle fn fill_alias(count: int) -> int {
    let original = native_buffer_int(count)
    let buf = original
    for i in 0..count { buf[i] = i }
    return original.len
}
"#,
    );
    assert!(!function_body(&lua, "fill_alias").contains("__nb_checked"));
}

#[test]
fn constructor_bounds_do_not_survive_mutation_or_foreign_calls() {
    for setup in [
        "let alias = buf; shrink(alias)",
        "shrink(buf)",
        "count += 1",
        "if count > 0 { buf = native_buffer_int(0) }",
    ] {
        let lua = emit_module(&format!(
            r#"
inline(lua) fn shrink(buf: NativeBuffer<int>) = "buf.len = 0"
@nomangle fn changed(count: int) -> int {{
    let buf = native_buffer_int(count)
    {setup}
    for i in 0..count {{ buf[i] = i }}
    return 1
}}
"#
        ));
        assert!(
            function_body(&lua, "changed").contains("__nb_checked"),
            "{setup}"
        );
        assert_eq!(
            run_luajit(&format!(
                r#"{lua}
local ok, err = pcall(changed, 4)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
            ))
            .trim(),
            "ok",
            "{setup}"
        );
    }
}

#[test]
fn a_recomputed_capacity_does_not_follow_mutated_inputs() {
    let lua = emit_module(
        r#"
struct Size { width: int }
inline(lua) fn grow(size: Size) = "size.width = size.width + 1"
@nomangle fn changed_size(size: Size) -> int {
    let buf = native_buffer_int(size.width)
    grow(size)
    for i in 0..size.width { buf[i] = i }
    return 1
}
"#,
    );
    assert!(function_body(&lua, "changed_size").contains("__nb_checked"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local ok, err = pcall(changed_size, {{width=4}})
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}

#[test]
fn reexecuted_constructor_does_not_resize_an_old_alias() {
    let lua = emit_module(
        r#"
@nomangle fn old_buffer() -> int {
    let buf = native_buffer_int(1)
    for count in 1..3 {
        let old = buf
        buf = native_buffer_int(count)
        for i in 0..count { old[i] = i }
    }
    return 1
}
"#,
    );
    assert!(function_body(&lua, "old_buffer").contains("__nb_checked"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local ok, err = pcall(old_buffer)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}

use crate::common;
use std::collections::HashMap;

fn run(src: &str, driver: &str) -> String {
    let ws = common::ws_with_stdlib(src);
    let mid = common::module_id(&ws, "<test>");
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("emit")
        .lua;
    let functions = &ws.module(mid).functions;
    let mut counts: HashMap<_, usize> = HashMap::new();
    for (_, f) in functions.iter() {
        *counts.entry(f.name).or_default() += 1;
    }
    let mut aliases = String::new();
    for (_, f) in functions.iter() {
        let mangled = skull_mir::mangle::mangle_fn(&ws, f);
        let source_name = bone_core::modules::display_name(f.name.as_str());
        if counts[&f.name] == 1 && mangled != source_name {
            aliases.push_str(&format!("_G[\"{source_name}\"] = {mangled}\n"));
        }
    }
    common::run_luajit(&format!("{lua}\n{aliases}print({driver})\n"))
}

#[test]
fn callee_binding_keeps_calls_to_the_right_function() {
    let _inline_off = common::inline_off_guard();
    for (src, driver, expected) in [
        (
            "fn discarded(a: int) -> int { a + 2 }\nfn live(a: int) -> int { a + 1 }\nfn caller(a: int) -> int { if false { discarded(a) } else { live(a) } }",
            "caller(7)",
            "8",
        ),
        (
            "fn caller(value: float, count: uint) -> uint { if false { (value as uint) << count } else { count } }",
            "caller(-1, 3)",
            "3",
        ),
        (
            "fn leaf(a: int) -> int { a + 1 }\n@nomangle fn b0(a: int) -> int { a + 2 }\nfn caller(a: int) -> int { leaf(a) + b0(a) }",
            "caller(3)",
            "9",
        ),
        (
            "fn leaf(a: int) -> int { a + 1 }\n@inline inline(lua) fn raw(a: int) -> int = \"return b0(a)\"\nfn caller(a: int) -> int { leaf(a) + raw(a) }",
            "(function() b0 = function(a) return a + 10 end; return caller(3) end)()",
            "17",
        ),
        (
            "fn leaf(a: int) -> int { a + 1 }\nfn other(a: int) -> int { a * 3 }\n@inline inline(lua) fn twice(a: int) -> int = \"local b0 = a * 2\\nreturn b0\"\nfn caller(n: int) -> int {\n  let s = 0\n  for i in 0..n { s = s + leaf(twice(i)) + other(i) }\n  return s\n}",
            "caller(4)",
            "34",
        ),
    ] {
        assert_eq!(run(src, driver), expected, "{src}");
    }
}

#[test]
fn callee_binding_caps_at_the_lua_upvalue_limit() {
    let _inline_off = common::inline_off_guard();
    let count = skull_codegen_lua::MAX_BOUND_CALLEES + 7;
    let mut src = String::new();
    for i in 0..count {
        src.push_str(&format!("fn leaf{i:03}(a: int) -> int {{ a + {i} }}\n"));
    }
    src.push_str("fn caller(a: int) -> int {\n  let t = 0\n");
    for i in 0..count {
        src.push_str(&format!("  t = t + leaf{i:03}(a)\n"));
    }
    src.push_str("  t\n}\n");
    let expected: i64 = (0..count as i64).map(|i| 7 + i).sum();
    assert_eq!(run(&src, "caller(7)"), expected.to_string());
}

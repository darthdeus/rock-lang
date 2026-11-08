//! deepcopy emission (T8d): `deepcopy` maps to the `__rock_deepcopy_value`
//! runtime (`JS_DEEPCOPY_RUNTIME`) -- a `seen`-Map clone that preserves the
//! object graph (shared refs copy once, cycles terminate). Node execution is the
//! correctness oracle; `tests/library/js_deepcopy.rock` cross-checks against luajit.

use crate::common::ws_with;
use skull_mir::mir::FuncRef;

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef {
                module: mid,
                func: fid,
            });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn eval_int(defs: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_DEEPCOPY_RUNTIME,
    );
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result\n{program}");
}

// A shared inner struct copies once: mutating via one field shows through the
// other, and the original is untouched (99*100 + 1 encodes copy-then-original).
#[test]
fn shared_ref_copies_once() {
    let src = "struct Inner { v: int }\n\
               struct State { a: Inner, b: Inner }\n\
               fn root() -> int {\n\
                   let inner = Inner { v: 1 }\n\
                   let s = State { a: inner, b: inner }\n\
                   let c = deepcopy(s)\n\
                   c.a.v = 99\n\
                   c.b.v * 100 + s.a.v\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("deepcopy emits");
    assert!(js.contains("deepcopy("), "renders deepcopy:\n{js}");
    eval_int(&js, "root", 9901);
}

// A self-referential struct copies without hanging; mutating the copy is visible
// through its own back-reference, and the original is untouched.
#[test]
fn cycle_copies_without_hanging() {
    let src = "struct Node { tag: int, next: Node? }\n\
               fn root() -> int {\n\
                   let n = Node { tag: 1, next: null }\n\
                   n.next = n\n\
                   let c = deepcopy(n)\n\
                   c.tag = 5\n\
                   let via = if c.next != null { c.next.tag } else { 0 }\n\
                   via * 100 + n.tag\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("cyclic deepcopy emits");
    eval_int(&js, "root", 501);
}

// A deep-copied Vec is independent of the original.
#[test]
fn vec_copy_is_independent() {
    let src = "fn root() -> int {\n\
                   let v = [1, 2, 3]\n\
                   let c = deepcopy(v)\n\
                   c.set(0, 10)\n\
                   c[0] * 100 + v[0]\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("vec deepcopy emits");
    eval_int(&js, "root", 1001);
}

#[test]
fn copied_code_ref_env_shares_logical_slot() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}\n\
         function old(e) {{ return e.c + 1; }}\n\
         function edited(e) {{ return e.c + 100; }}\n\
         __rock_code_slot_begin();\n\
         __rock_code_slot_stage_one(\"probe\", old, \"probe-abi\");\n\
         __rock_code_slot_publish();\n\
         var original = {{ __slot: __rock_code_slot_ref(\"probe\"), c: 7 }};\n\
         var copy = deepcopy(original);\n\
         __rock_code_slot_begin();\n\
         __rock_code_slot_stage_one(\"probe\", edited, \"probe-abi\");\n\
         __rock_code_slot_publish();\n\
         console.log(__rock_call(copy));",
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_DEEPCOPY_RUNTIME,
    );
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "107",
        "{program}"
    );
}

// A column-stored Vec cannot enter the opaque `clone_into` body, which would
// walk `_data`; the shared lowering copies each column instead, as on luajit.
// The copy owns its columns: writing the original afterwards leaves it alone.
#[test]
fn clone_into_copies_column_vec_storage() {
    let src = "fn root() -> int {\n\
                   let owner: [int] = []\n\
                   let points = [Vec2 { x: 1.0, y: 2.0 }, Vec2 { x: 3.0, y: 4.0 }]\n\
                   let copied = points.clone_into(owner.storage)\n\
                   points[0] = Vec2 { x: 9.0, y: 10.0 }\n\
                   copied.push(Vec2 { x: 5.0, y: 6.0 })\n\
                   return int(copied[0].x) * 100 + copied.len() * 10 + points.len()\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("column clone_into emits");
    eval_int(&js, "root", 132);
}

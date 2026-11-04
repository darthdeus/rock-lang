//! GC-free value-struct emission (T8b). An all-scalar `value` struct used as a
//! WHOLE value (a `MakeValueStruct` that survives cleanup -- `==`, a Vec element,
//! a boxed return) emits as a plain `{__ty, ...}` object; its structural deep-eq
//! (`JS_STRUCTURAL_EQ`) reproduces luajit's byte-value equality for all-scalar
//! leaves. A GC-carrying value struct rejects (its pointer fields compare by
//! REFERENCE on luajit, which a generic contents-eq cannot reproduce). The
//! node-executed checks are the correctness oracle.

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
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result\n{program}");
}

// Two distinct constructions with equal contents compare EQUAL, unequal contents
// compare UNEQUAL -- structural deep-eq matching luajit byte-value eq.
#[test]
fn all_scalar_value_struct_eq_by_contents() {
    let src = "value V { x: int, y: int }\n\
               fn root() -> int {\n\
                   let a = V { x: 1, y: 2 }\n\
                   let b = V { x: 1, y: 2 }\n\
                   let c = V { x: 1, y: 3 }\n\
                   let eq = if a == b { 1 } else { 0 }\n\
                   let ne = if a == c { 1 } else { 0 }\n\
                   eq * 10 + ne\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("all-scalar value struct emits");
    assert!(
        js.contains("__ty: \"V\""),
        "value struct emits a `{{__ty, ...}}` object:\n{js}"
    );
    eval_int(&js, "root", 10); // a==b -> 1, a==c -> 0
}

// A nested all-scalar value struct emits nested `{__ty, ...}` objects and its
// deep-eq is by contents at every level.
#[test]
fn nested_value_struct_eq() {
    let src = "value V { x: int, y: int }\n\
               value Line { a: V, b: V }\n\
               fn root() -> int {\n\
                   let p = Line { a: V { x: 1, y: 2 }, b: V { x: 3, y: 4 } }\n\
                   let q = Line { a: V { x: 1, y: 2 }, b: V { x: 3, y: 4 } }\n\
                   if p == q { 1 } else { 0 }\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("nested value struct emits");
    eval_int(&js, "root", 1);
}

// A GC-carrying value struct (a boxed string field) now emits (T8g): the
// per-type `__rock_vt_eq` registry supplies the field split -- a `string` field
// compares BY REFERENCE (luajit interns; JS `===` is by-value), a scalar field
// structurally. Two boxes with equal string + scalar compare EQUAL.
#[test]
fn gc_carrying_value_struct_eq() {
    let src = "value W { s: string, n: int }\n\
               fn root() -> int {\n\
                   let a = W { s: \"hi\", n: 1 }\n\
                   let b = W { s: \"hi\", n: 1 }\n\
                   if a == b { 1 } else { 0 }\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("GC-carrying value struct emits (T8g)");
    assert!(
        js.contains(
            "__rock_vt_eq[\"W\"] = function(a, b) { return a.s === b.s && __op_eq(a.n, b.n); };"
        ),
        "emits the W registry entry (string ref, scalar structural):\n{js}"
    );
    eval_int(&js, "root", 1);
}

/// A `nobox` return crosses the JS call boundary as LEAVES, not as anything the
/// guarantee forbids. This replaced a blanket rejection, so it asserts the
/// property that rejection was protecting rather than the rejection itself: the
/// value must not materialize as an object OR as a transport array (which is an
/// allocation just the same, and does not vanish under V8's escape analysis).
#[test]
fn nobox_value_return_crosses_as_leaves_not_a_box() {
    let src = "nobox value V { x: int, y: int }\n\
               fn mk() -> V { V { x: 1, y: 2 } }\n\
               fn root() -> int {\n\
                   let v = mk()\n\
                   return v.x * 10 + v.y\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("a nobox return emits under the scratch ABI");
    assert!(
        !js.contains("\"V\""),
        "nobox value materialized an object:\n{js}"
    );
    assert!(
        !js.contains("return ["),
        "nobox return allocated a transport array:\n{js}"
    );
    assert!(
        js.contains("__rock_ret1 = "),
        "leaf 1 must ride the scratch bank:\n{js}"
    );
    eval_int(&js, "root", 12);
}

#[test]
fn ordinary_value_with_a_flat_enum_return_crosses_as_leaves() {
    let src = "enum Payload { A, B }\n\
               enum Kind { Plain, Payload(Payload) }\n\
               value Row { kind: Kind, key: string, count: int }\n\
               fn make(kind: Kind, n: int) -> Row {\n\
                   return match kind {\n\
                       Kind::Plain => Row { kind, key: \"plain\", count: n + 1 },\n\
                       Kind::Payload(_) => Row { kind, key: \"payload\", count: n + 1 },\n\
                   }\n\
               }\n\
               fn consume(row: Row) -> int {\n\
                   let key_bonus = if row.key == \"payload\" { 10 } else { 0 }\n\
                   return match row.kind {\n\
                       Kind::Plain => row.count + key_bonus,\n\
                       Kind::Payload(payload) => match payload {\n\
                           Payload::A => row.count + key_bonus,\n\
                           Payload::B => row.count + key_bonus + 4,\n\
                       },\n\
                   }\n\
               }\n\
               fn root() -> int { consume(make(Kind::Payload(Payload::B), 4)) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("ordinary nested-enum value emits");
    assert!(
        !js.contains("__ty: \"Row\""),
        "the returned row materialized an object:\n{js}"
    );
    eval_int(&js, "root", 19);
}

// A `value` struct goes OFF cdata once it carries a nullable `nobox` child, so
// its box keys every inline child's leaves by dotted path (`pos.x`, `pos.y`, and
// a presence tag at the child's own name). Readers are lowered against exactly
// those keys (`Inst::Field { field: "pos.x" }`), so a box that escapes -- a Vec
// element, an argument -- must be BUILT with them: a nested object under `pos`
// leaves every read undefined.
#[test]
fn a_leaf_table_value_box_keys_its_inline_children_by_dotted_path() {
    let src = "nobox value Id { value: int }\n\
               value V2 { x: int, y: int }\n\
               value Row { id: int, tag: Id? = null, pos: V2 }\n\
               fn root() -> int {\n\
                   let rows: [Row] = []\n\
                   rows.push(Row { id: 1, tag: Id { value: 5 }, pos: V2 { x: 2, y: 3 } })\n\
                   let p = rows[0].pos\n\
                   let tag = rows[0].tag.unwrap()\n\
                   return p.x * 100 + p.y * 10 + tag.value + rows[0].id\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("leaf-table value stored in a Vec emits");
    assert!(
        js.contains("\"pos.x\""),
        "the box must key the inline child by leaf path, not box it under `pos`:\n{js}"
    );
    eval_int(&js, "root", 236);
}

const REFLECTED_NULLABLE: &str = r#"
nobox value Place { area: int, x: int, y: int }
struct Owner { place: Place?, other: Place?, reads: int }
nobox value Flag { enabled: bool, place: Place? }
struct Flags { flag: Flag? }
fn reflected_read<T, V>(owner: T, field: ReadFieldRef<T, V>) -> V {
    return field_get(owner, field)
}
fn owner_once(owner: Owner) -> Owner {
    owner.reads += 1
    return owner
}
"#;

#[test]
fn nullable_field_get_evaluates_owner_once_and_copies() {
    let src = format!(
        "{REFLECTED_NULLABLE}{}",
        r#"
fn root() -> int {
    let owner = Owner { place: Place { area: 3, x: 5, y: 7 }, other: null, reads: 0 }
    let snapshot = field_get(f: .place, o: owner_once(owner)).unwrap()
    owner.place = Place { area: 11, x: 13, y: 17 }
    if owner.reads == 1 && snapshot.area == 3 && snapshot.x == 5 && snapshot.y == 7 { 1 } else { 0 }
}
"#
    );
    let ws = ws_with(&src);
    eval_int(
        &emit_closure(&ws, "root").expect("nullable field_get emits"),
        "root",
        1,
    );
}

#[test]
fn nullable_field_get_preserves_false_and_nested_presence() {
    let src = format!(
        "{REFLECTED_NULLABLE}{}",
        r#"
fn root() -> int {
    let owner = Flags { flag: Flag { enabled: false, place: null } }
    let read = reflected_read(owner, .flag).unwrap()
    if read.enabled || read.place != null { return 0 }
    owner.flag = Flag { enabled: true, place: Place { area: 19, x: 23, y: 29 } }
    let present = reflected_read(owner, .flag).unwrap()
    if !present.enabled || present.place.unwrap().x != 23 { return 0 }
    owner.flag = null
    if reflected_read(owner, .flag) == null { 1 } else { 0 }
}
"#
    );
    let ws = ws_with(&src);
    eval_int(
        &emit_closure(&ws, "root").expect("nested nullable field_get emits"),
        "root",
        1,
    );
}

#[test]
fn nullable_field_get_reads_present_absent_and_reassigned_payloads() {
    let src = format!(
        "{REFLECTED_NULLABLE}{}",
        r#"
fn root() -> int {
    let owner = Owner { place: Place { area: 7, x: 11, y: 13 }, other: null, reads: 0 }
    let read = reflected_read(owner, .place).unwrap()
    if read.area != 7 || read.x != 11 || read.y != 13 { return 0 }
    if reflected_read(owner, .other) != null { return 0 }
    owner.place = null
    if reflected_read(owner, .place) != null { return 0 }
    owner.place = Place { area: 17, x: 19, y: 23 }
    let reread = reflected_read(owner, .place).unwrap()
    if reread.area == 17 && reread.x == 19 && reread.y == 23 { 1 } else { 0 }
}
"#
    );
    let ws = ws_with(&src);
    eval_int(
        &emit_closure(&ws, "root").expect("nullable field_get emits"),
        "root",
        1,
    );
}

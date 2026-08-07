//! SENTINEL. `reject_nobox_boxed_call_args` checks ARGUMENT positions only, so
//! a generic `-> V` instantiated at a `nobox` value crosses an opaque boundary
//! unrejected and reads a key that does not exist.
//!
//! A `nobox` value field is FLATTENED into per-leaf keys: `struct Row { pos:
//! Vec2 }` emits `{["pos.x"] = .., ["pos.y"] = ..}`, with no `pos` key at all.
//! An `inline(lua)` body is an opaque string, so nothing adapts it to that
//! representation -- exactly the reasoning in
//! `inline_lua_column_container_param_sentinel.rs`, one position over.
//!
//! The two directions disagree, which is the whole finding. `field_set` takes
//! its value as a PARAM, so the arg check fires and the program is rejected
//! with "nobox value `Vec2` cannot cross boxed call boundary `field_set`".
//! `field_get` hands the same value back as a generic RETURN, nothing checks
//! it, and the emitted `o["pos"]` is nil -- a crash on a read, and in the
//! stdlib's most reachable descriptor primitive.
//!
//! DISCRIMINANT, measured against the nearest working controls: it is not
//! genericity and not struct-typedness. A plain `struct` field and an UNMARKED
//! `value` field both round-trip through the identical generic `field_get`; a
//! `nobox value` field is the only one that dies. The `nobox` mark is what
//! makes the field flatten, so it is what removes the key the boundary reads.
//!
//! NOT the same hole as the declared-`embed` rejection in
//! `bone-inference/src/expr.rs`, which refuses `ReadFieldRef`/`FieldRef` to an
//! `embed` field. That predicate asks whether the field was SPELLED `embed`,
//! not whether it is flattened, so a `nobox`-typed field walks straight past
//! it. `tests/errors/embed_projection_only.rock.err` pins the spelled form and
//! is unaffected by this.
//!
//! WHEN THIS GOES RED the rejection grew a return-position sibling: flip these
//! two tests to expect a compile error naming the boundary, keep the three
//! controls exactly as they are, and note that
//! `docs/scene-system-plan.md` P4's FieldRef pull model is unblocked -- a
//! position descriptor is `ReadFieldRef<T, Vec2>`, which is precisely this
//! shape.
//!
//! The rejection's SHAPE is the open question, not its necessity. It must not
//! be "opaque callee whose return type is nobox": stdlib constructors like
//! `v(x, y)` are `@intrinsic inline(lua) -> Vec2` and are legitimate, so a
//! blanket return check detonates the tree. The narrow reading -- a return
//! spelled as a TYPE PARAMETER that a substitution lands on a nobox -- is what
//! separates `field_get` from `v`.

/// The repro through the stdlib primitive that actually bites.
const FIELD_GET_NOBOX: &str = r#"
struct Row { pos: Vec2, tag: int }

fn read_pos(r: Row, f: ReadFieldRef<Row, Vec2>) -> float {
    return field_get(r, f).x
}

fn main() {
    let r = Row { pos: v(1.0, 2.0), tag: 0 }
    print("{read_pos(r, .pos)}")
}
"#;

/// The same boundary with no `FieldRef` anywhere: a hand-written generic
/// `inline(lua)` returning `V`. Proves the hole is the RETURN POSITION rather
/// than anything the field-descriptor types do.
const GENERIC_RETURN_NOBOX: &str = r#"
struct Box2 { p: Vec2 }

@inline inline(lua) fn unwrap_generic<V>(b: Box2) -> V = """
    return b.p
"""

fn main() {
    let a: Vec2 = unwrap_generic(Box2 { p: v(1.0, 2.0) })
    print("x={a.x}")
}
"#;

/// Control: a boxed `struct` field keeps its single key and round-trips.
const FIELD_GET_BOXED_STRUCT: &str = r#"
struct Pt { x: int, y: int }
struct Holder { p: Pt, tag: int }

fn read_p(h: Holder, f: ReadFieldRef<Holder, Pt>) -> int {
    return field_get(h, f).x
}

fn main() {
    let h = Holder { p: Pt { x: 7, y: 0 }, tag: 1 }
    print("{read_p(h, .p)}")
}
"#;

/// Control: an UNMARKED `value` field is not flattened, so the same generic
/// primitive is correct for it. This is the arm that rules out "value types
/// cannot be read through a descriptor".
const FIELD_GET_VALUE_STRUCT: &str = r#"
value VPt { x: int, y: int }
struct VHolder { p: VPt, tag: int }

fn read_p(h: VHolder, f: ReadFieldRef<VHolder, VPt>) -> int {
    return field_get(h, f).x
}

fn main() {
    let h = VHolder { p: VPt { x: 9, y: 0 }, tag: 1 }
    print("{read_p(h, .p)}")
}
"#;

/// Control: the WRITE direction of the very same field, which IS rejected.
/// Deleting the arg check would turn this green and is how you tell a real fix
/// from the check being lost.
const FIELD_SET_NOBOX: &str = r#"
struct Row { pos: Vec2, tag: int }

fn write_pos(r: Row, f: WriteFieldRef<Row, Vec2>, val: Vec2) {
    field_set(r, f, val)
}

fn main() {
    let r = Row { pos: v(1.0, 2.0), tag: 0 }
    write_pos(r, .pos, v(99.0, 98.0))
    print("{r.pos.x}")
}
"#;

fn emit_err(src: &str) -> String {
    match crate::common::try_emit_module(src) {
        Ok(_) => String::new(),
        Err(e) => format!("{e}"),
    }
}

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn pinned_bug_field_get_of_a_nobox_field_is_admitted() {
    assert_eq!(
        emit_err(FIELD_GET_NOBOX),
        "",
        "PINNED BEHAVIOR (BUG): `field_get` returning a flattened `nobox` field \
         is accepted. The field emits as `pos.x`/`pos.y`, so the body's \
         `o[\"pos\"]` is nil. If this now errors, the return-position rejection \
         landed -- assert the error names the boundary instead."
    );
}

#[test]
fn pinned_bug_a_generic_nobox_return_crosses_an_opaque_boundary() {
    assert_eq!(
        emit_err(GENERIC_RETURN_NOBOX),
        "",
        "PINNED BEHAVIOR (BUG): a generic `-> V` instantiated at a `nobox` \
         value crosses an `inline(lua)` boundary unchecked, with no `FieldRef` \
         involved. If this now errors, flip it."
    );
}

#[test]
fn a_boxed_struct_field_reads_correctly_through_the_same_primitive() {
    assert_eq!(
        run(FIELD_GET_BOXED_STRUCT, "fieldref_boxed_struct"),
        "7",
        "a boxed field keeps one key and must stay readable through `field_get`"
    );
}

#[test]
fn an_unmarked_value_field_reads_correctly_through_the_same_primitive() {
    assert_eq!(
        run(FIELD_GET_VALUE_STRUCT, "fieldref_value_struct"),
        "9",
        "an unmarked `value` field is not flattened, so it must stay readable"
    );
}

#[test]
fn the_write_direction_of_the_same_field_is_rejected() {
    let err = emit_err(FIELD_SET_NOBOX);
    assert!(
        err.contains("cannot cross boxed call boundary"),
        "the arg-position check must keep rejecting `field_set` of a nobox \
         field; this is the half that works, and the asymmetry against \
         `field_get` is the finding. got: {err:?}"
    );
}

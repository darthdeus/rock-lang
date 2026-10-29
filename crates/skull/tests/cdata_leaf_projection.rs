use crate::common::{emit_module, run_luajit};

const SOURCE: &str = r#"
nobox value Cell { x: int, y: int }
nobox value Location { region: int, cell: Cell }
value Motion { active: bool, at: Location, weight: float, __rock_leaf_: int = 19 }
struct Holder { motion: Motion }
value Pair { x: float, y: float }
value Floating { pair: Pair, other: float }
value FloatingPair { pair: Pair }
@nomangle fn copied(owner: Holder) -> Motion { return materialize(owner.motion) }
@nomangle fn floating() -> Floating {
    return materialize(Floating { pair: Pair { x: 1.25, y: 2.5 }, other: 5.0 })
}
@nomangle fn floating_pair() -> FloatingPair {
    return materialize(FloatingPair { pair: Pair { x: 1.25, y: 2.5 } })
}
@nomangle fn make() -> Holder {
    return Holder { motion: Motion { active: true,
        at: Location { region: 7, cell: Cell { x: 3, y: 5 } }, weight: 2.5 } }
}
@nomangle fn read(owner: Holder) -> int {
    return owner.motion.at.region + owner.motion.at.cell.x + owner.motion.at.cell.y
}
fn borrowed(state: ref Motion) -> int {
    return state.at.region + state.at.cell.x + state.at.cell.y
}
@nomangle fn change(owner: Holder) {
    owner.motion.at.cell.x = 11
    assert(borrowed(owner.motion) == 23, "borrow sees the primitive write")
    let before = owner.motion.at
    owner.motion.at.cell.y = 13
    assert(before.cell.y == 5, "a retained value remains a snapshot")
    assert(owner.motion.weight == 2.5 && owner.motion.active && owner.motion.__rock_leaf_ == 19,
        "mixed-width fields retain their native offsets")
}
"#;

#[test]
fn nested_cdata_scalar_reads_allocate_no_intermediate_descriptors() {
    let lua = emit_module(SOURCE);
    let dir = std::env::var_os("HOME").map(std::path::PathBuf::from)
        .expect("HOME for disk-backed ABI fixture").join("tmp")
        .join(format!("rock-cdata-abi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("ABI fixture directory");
    let source = dir.join("abi.c");
    let library = dir.join("abi.so");
    std::fs::write(&source, r#"
#include <stdbool.h>
typedef struct { int x, y; } Cell;
typedef struct { int region; Cell cell; } Location;
typedef struct { bool active; Location at; double weight; int marker; } Motion;
typedef struct { double x, y; } Pair;
typedef struct { Pair pair; double other; } Floating;
typedef struct { Pair pair; } FloatingPair;
double s19_motion(Motion v) {
    return v.at.region + v.at.cell.x + v.at.cell.y + v.weight + v.marker;
}
double s19_floating(Floating v) { return v.pair.x + v.pair.y + v.other; }
double s19_floating_pair(FloatingPair v) { return v.pair.x + v.pair.y; }
"#).expect("write independent C layout");
    let built = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-o"]).arg(&library).arg(&source)
        .output().expect("compile independent ABI fixture");
    assert!(built.status.success(), "C ABI fixture: {}", String::from_utf8_lossy(&built.stderr));
    let library = library.to_str().expect("UTF-8 fixture path");
    let program = format!(r#"{lua}
ffi.cdef[[double s19_motion(Motion v); double s19_floating(Floating v);
double s19_floating_pair(FloatingPair v);]]
local native = ffi.load('{library}')
local owner = make()
assert(native.s19_motion(copied(owner)) == 36.5)
assert(native.s19_floating(floating()) == 8.75)
assert(native.s19_floating_pair(floating_pair()) == 3.75)
assert(native.s19_motion(Motion_ct(true, Location_ct(7, Cell_ct(3, 5)), 2.5, 19)) == 36.5)
assert(native.s19_floating(Floating_ct(Pair_ct(1.25, 2.5), 5.0)) == 8.75)
assert(native.s19_floating_pair(FloatingPair_ct(Pair_ct(1.25, 2.5))) == 3.75)
jit.off()
jit.flush()
collectgarbage('collect')
collectgarbage('stop')
local before = collectgarbage('count')
for i = 1, 8192 do assert(read(owner) == 15) end
local growth = collectgarbage('count') - before
assert(growth < 4, 'nested scalar reads allocate descriptors: ' .. growth .. ' KiB')
change(owner)
print(read(owner))
"#);
    let output = run_luajit(&program, "cdata_leaf_projection");
    std::fs::remove_dir_all(&dir).expect("remove ABI fixture");
    assert_eq!(output, "31");
}

#[test]
fn borrowing_an_inline_value_without_a_holder_is_rejected() {
    let source = r#"
nobox value Point { x: int, y: int }
struct Owner { point: Point }
fn main() { let owner = Owner { point: Point { x: 3, y: 5 } };
    let point: ref Point = owner.point; print(point.x) }
"#;
    let error = crate::common::try_emit_module(source)
        .expect_err("an inline value has leaves but no holder to borrow");
    assert!(error.message.contains("cannot borrow an inline value field"), "{error:?}");
}

#[test]
fn native_value_iteration_snapshots_without_nested_descriptors() {
    let lua = emit_module(&format!(r#"{SOURCE}
@nomangle fn rows() -> [Motion] {{ return [make().motion] }}
@nomangle fn scan(rows: [Motion]) -> int {{
    let total = 0
    for row in rows {{ if row.active {{ total += row.at.region }} }}
    return total
}}
@nomangle fn snapshot(rows: [Motion]) -> int {{
    let before = rows[0]
    rows[0].at.cell.y = 101
    return before.at.cell.y
}}
@nomangle fn loop_snapshot(rows: [Motion]) -> int {{
    let total = 0
    for row in rows {{ rows[0].at.cell.y = 37; total += row.at.cell.y }}
    return total
}}
"#));
    let program = format!(r#"{lua}
local values = rows()
assert(scan(values) == 7)
assert(snapshot(values) == 5)
assert(loop_snapshot(values) == 101)
assert(snapshot(values) == 37)
assert(scan(values) == 7)
jit.off(); jit.flush()
for i = 1, 100 do assert(scan(values) == 7) end
collectgarbage('collect'); collectgarbage('stop')
local before = collectgarbage('count')
for i = 1, 8192 do assert(scan(values) == 7) end
local growth = collectgarbage('count') - before
collectgarbage('restart')
assert(growth < 4, 'value iteration allocates nested descriptors: ' .. growth .. ' KiB')
print('ok')
"#);
    assert_eq!(run_luajit(&program, "cdata_iteration"), "ok");
}

#[test]
fn nullable_native_field_snapshots_avoid_intermediate_descriptors() {
    let lua = emit_module(r#"
value Point { x: int, y: int }
value Span { a: Point, b: Point }
value State { active: bool, span: Span? }
struct Owner { state: State }
fn ready(state: State) -> bool { state.active }
@nomangle fn snapshot(owner: Owner) -> bool {
    let state = owner.state
    return ready(state)
}
@nomangle fn make(present: bool) -> Owner {
    let span = if present {
        Span { a: Point { x: 2, y: 3 }, b: Point { x: 5, y: 7 } }
    } else { null }
    return Owner { state: State { active: true, span: span } }
}
@nomangle fn retained(owner: Owner) -> int {
    let state = owner.state
    owner.state.span = null
    if let span = state.span { return span.a.x + span.a.y + span.b.x + span.b.y }
    return -1
}
"#);
    let program = format!(r#"{lua}
local present, absent = make(true), make(false)
assert(snapshot(present) and snapshot(absent))
assert(retained(present) == 17 and retained(present) == -1)
assert(retained(absent) == -1)
assert(not pcall(snapshot, nil), 'invalid owner read must still fail')
present = make(true)
jit.off(); jit.flush()
for i = 1, 100 do assert(snapshot(present) and snapshot(absent)) end
collectgarbage('collect'); collectgarbage('stop')
local before = collectgarbage('count')
for i = 1, 8192 do assert(snapshot(present) and snapshot(absent)) end
local growth = collectgarbage('count') - before
collectgarbage('restart')
assert(growth < 4, 'nullable native snapshot allocates child descriptors: ' .. growth .. ' KiB')
print('ok')
"#);
    assert_eq!(run_luajit(&program, "nullable_native_snapshot"), "ok");
}

#[test]
fn optional_inline_child_projection_keeps_constructor_folding() {
    let lua = emit_module(r#"
nobox value Point { x: int, y: int }
nobox value Inner { point: Point, tag: int }
nobox value SnapshotEnvelope { inner: Inner, flag: int }
struct Owner { envelope: SnapshotEnvelope? }
fn child(owner: Owner) -> Inner? { return owner.envelope?.inner }
fn main() {
    let owner = Owner { envelope: SnapshotEnvelope {
        inner: Inner { point: Point { x: 2, y: 3 }, tag: 7 }, flag: 11 } }
    let before = child(owner)
    owner.envelope = null
    let value = before.unwrap()
    print("{value.point.x} {value.point.y} {value.tag} {child(owner) == null}")
}
"#);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "optional_inline_child"), "2 3 7 true");
}

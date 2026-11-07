//! An absent nullable VALUE placed in a tuple must read as absent.
//!
//! `let t = (m.get(k), 1)` on a MISSING key, then `t.0 == null`, once answered
//! `false` -- the whole body folded to `do return (not (true)) end`, because a
//! boxed tuple slot got the ctor `pack` builds regardless of presence.
//!
//! Same root as the safe-nav receiver defect (`tests/nullable/safe_nav_absent_value_receiver.rock`)
//! -- a presence tag dropped on the way to a box -- and the same shape of fix,
//! at the CONSUMER rather than in `pack`. It is NOT fixable in `pack`'s Group
//! arm, the obvious-looking move: branching there puts the ctor behind a cell,
//! SROA stops forwarding it, the box survives to the residue check, and
//! `nobox_coalesce_at_ctor_field` regresses CORRECT -> LOUD REJECT. Narrowing to
//! a static presence does not help either -- at a `??` the presence is dynamic
//! and the coalesce consumes it anyway. A boxed tuple slot is already an escape,
//! so `pack_maybe_absent` there costs no forwarding that survived anyway.
//!
//! Tuple PROJECTION needed no separate fix: with `nil` genuinely stored in the
//! slot, reading it back is the ordinary boxed path.
//!
//! Nothing here is `nobox` -- a plain unmarked `value` reproduces it, which is
//! the wider exposure. The `struct` twin bounds the class.

use std::process::Command;

fn run_capturing(prog: &str, tag: &str) -> (bool, String) {
    let path =
        std::env::temp_dir().join(format!("skull_sentinel_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit")
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

fn run_module(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), tag);
    assert!(ok, "program must run:\n{out}");
    out.trim().to_string()
}

const VALUE_IN_TUPLE: &str = r#"
value NV2 { x: float, y: float }

fn probe(m: Map<int, NV2>, k: int) -> bool {
    let t = (m.get(k), 1)
    return t.0 == null
}

fn main() {
    let m: Map<int, NV2> = map_new()
    m[1] = NV2 { x: 1.0, y: 2.0 }
    print("{probe(m, 1)} {probe(m, 9)}")
}
"#;

/// Bounds the class: a reference struct in the same position is correct, so this
/// is the value-struct presence tag, not tuples or `Map.get`.
const STRUCT_IN_TUPLE: &str = r#"
struct SV2 { x: float, y: float }

fn probe(m: Map<int, SV2>, k: int) -> bool {
    let t = (m.get(k), 1)
    return t.0 == null
}

fn main() {
    let m: Map<int, SV2> = map_new()
    m[1] = SV2 { x: 1.0, y: 2.0 }
    print("{probe(m, 1)} {probe(m, 9)}")
}
"#;

#[test]
fn an_absent_value_in_a_tuple_reads_as_absent() {
    assert_eq!(
        run_module(VALUE_IN_TUPLE, "nullable_value_tuple"),
        "false true",
        "an absent nullable value in a tuple must report absence, like the struct control"
    );
}

#[test]
fn a_struct_in_a_tuple_is_correct() {
    assert_eq!(
        run_module(STRUCT_IN_TUPLE, "nullable_struct_tuple"),
        "false true",
        "a reference struct in a tuple must report absence correctly"
    );
}

//! The Lua wide-value calling convention.
//!
//! An ORDINARY `value` whose PAYLOAD leaf count exceeds
//! `WIDE_VALUE_PAYLOAD_LEAF_CUT` crosses a Rock-to-Rock Lua call as ONE slot
//! (its existing boxed form) instead of N scalar lanes, and a signature whose
//! surviving scalar lanes exceed `SIGNATURE_SCALAR_LANE_BUDGET` boxes its
//! widest remaining ordinary values until it fits. A `nobox` value is never
//! boxed by either rule and counts toward no budget decision: the only
//! rejection it can meet is the emit-time frame-limit check.
//!
//! These tests assert the EMITTED SHAPE -- the emitted Lua parameter lists --
//! not merely that the program runs, because "it runs" is equally true of the
//! old all-scalar wire for every case below 250 frame slots.

use skull_mir::lower::{SIGNATURE_SCALAR_LANE_BUDGET, WIDE_VALUE_PAYLOAD_LEAF_CUT};

use crate::common::{
    definition_names, emit_module_without_frame_limits, function_body, luajit_loads, run_main,
    try_emit_module,
};

/// The emitted Lua parameter list of the definition `name` names.
fn emitted_params<'a>(lua: &'a str, name: &str) -> Vec<&'a str> {
    let body = function_body(lua, name);
    let open = body.find('(').expect("a parameter list");
    let close = body[open..].find(')').expect("a closed parameter list") + open;
    let inner = body[open + 1..close].trim();
    if inner.is_empty() {
        return Vec::new();
    }
    inner.split(',').map(str::trim).collect()
}

/// `leaves` float fields, `f0 .. f{leaves-1}`.
fn float_fields(leaves: usize) -> String {
    (0..leaves)
        .map(|k| format!("f{k}: float"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn float_inits(leaves: usize, base: &str) -> String {
    (0..leaves)
        .map(|k| format!("f{k}: {base} + {k}.0"))
        .collect::<Vec<_>>()
        .join(", ")
}

// --- gate 1: the wide-uniform witness compiles, loads and computes ---------
//
// A retained wide-uniform shape: a 58-leaf ordinary
// `value` built once, then passed to and returned from helpers inside a loop,
// with other live locals in the caller. On the scalar wire the caller's frame
// is past `LJ_MAX_SLOTS` and the compiler rejects the body; the retained
// `generated-function.lua` witness keeps failing either way.

/// The 27-field / 58-leaf shape of an application's `DungeonRelightU`, in a form that
/// needs no application or host engine type: five 2-leaf `nobox` pairs, eleven 3-leaf
/// `nobox` triples, one 4-leaf `nobox` quad and eleven plain floats.
const WIDE_UNIFORM_SRC: &str = r#"
nobox value P2 { x: float, y: float }
nobox value P3 { x: float, y: float, z: float }
nobox value P4 { x: float, y: float, z: float, w: float }
value RelightU {
    origin: P2, extent: P2, space_origin: P2, grid_size: P2, cell: P2,
    count: float, resolution: float, samples: float, radius: float,
    strength: float, clock: float, cached: float,
    dynamic_active: float, first_caster: float, sprite_receiver: float,
    ambient: P3, sprite_origin: P3, sprite_dx: P3, sprite_dy: P3, sprite_dz: P3,
    sprite_ground: P3, sprite_position: P3, axis_x: P3, axis_y: P3, axis_z: P3,
    mesh_lo: P3,
    mesh_info: P4,
}

fn relight_base(seed: float) -> RelightU {
    return RelightU {
        origin: P2 { x: seed, y: seed }, extent: P2 { x: 1.0, y: 1.0 },
        space_origin: P2 { x: 0.0, y: 0.0 }, grid_size: P2 { x: 4.0, y: 4.0 },
        cell: P2 { x: 0.0, y: 0.0 },
        count: 0.0, resolution: 64.0, samples: 8.0, radius: 3.0,
        strength: 1.0, clock: 0.0, cached: 0.0,
        dynamic_active: 0.0, first_caster: 0.0, sprite_receiver: 0.0,
        ambient: P3 { x: 0.1, y: 0.1, z: 0.1 },
        sprite_origin: P3 { x: 0.0, y: 0.0, z: 0.0 },
        sprite_dx: P3 { x: 1.0, y: 0.0, z: 0.0 },
        sprite_dy: P3 { x: 0.0, y: 1.0, z: 0.0 },
        sprite_dz: P3 { x: 0.0, y: 0.0, z: 1.0 },
        sprite_ground: P3 { x: 0.0, y: 0.0, z: 0.0 },
        sprite_position: P3 { x: 0.0, y: 0.0, z: 0.0 },
        axis_x: P3 { x: 1.0, y: 0.0, z: 0.0 },
        axis_y: P3 { x: 0.0, y: 1.0, z: 0.0 },
        axis_z: P3 { x: 0.0, y: 0.0, z: 1.0 },
        mesh_lo: P3 { x: 0.0, y: 0.0, z: 0.0 },
        mesh_info: P4 { x: 0.0, y: 0.0, z: 0.0, w: 0.0 },
    }
}

fn sprite_uniform(index: float, base: RelightU) -> RelightU {
    return RelightU {
        ..base,
        sprite_position: P3 { x: index, y: index * 2.0, z: index * 3.0 },
        sprite_receiver: 1.0,
        count: base.count + 1.0,
    }
}

fn uniform_signal(u: RelightU) -> float {
    return u.count + u.sprite_position.x + u.sprite_receiver + u.origin.x
}

fn queue_sprites(seed: float, n: int) -> float {
    let base = relight_base(seed)
    let total = 0.0
    let lo = P2 { x: 0.0, y: 0.0 }
    let hi = P2 { x: 1.0, y: 1.0 }
    let span = P3 { x: 2.0, y: 2.0, z: 2.0 }
    for index in 0..n {
        let u = sprite_uniform(float(index), base)
        total += uniform_signal(u) + lo.x + hi.y + span.z
    }
    return total
}

fn main() { print("{queue_sprites(0.5, 3)}") }
"#;

#[test]
fn the_wide_uniform_witness_compiles_loads_and_computes() {
    let lua = try_emit_module(WIDE_UNIFORM_SRC).expect("the wide-uniform shape must emit");
    luajit_loads(&lua).expect("the emitted chunk must load");
    // Per iteration: count 1 + sprite_position.x (the index) + sprite_receiver 1
    // + origin.x 0.5, plus 0 + 1 + 2 from the three live narrow locals.
    // (2.5 + 3.5 + 4.5) + 3 * 3 = 19.5.
    assert_eq!(run_main(&lua), "19.5");
}

/// The 58-leaf ordinary value crosses as ONE slot, so the helper's emitted
/// parameter list is two entries, not 59.
#[test]
fn the_wide_uniform_helper_takes_one_slot_per_wide_value() {
    let lua = try_emit_module(WIDE_UNIFORM_SRC).expect("emits");
    assert_eq!(
        emitted_params(&lua, "sprite_uniform").len(),
        2,
        "the 58-leaf ordinary value must cross as one slot:\n{}",
        function_body(&lua, "sprite_uniform")
    );
    assert_eq!(
        emitted_params(&lua, "uniform_signal").len(),
        1,
        "the 58-leaf ordinary value must cross as one slot:\n{}",
        function_body(&lua, "uniform_signal")
    );
}

/// Gate 3: the boxed wide value CONTAINS `nobox` values. The callee must read
/// their fields out of the box without the implicit-materialization rejection
/// seen with nested uniform fields, and must be able to build a
/// new one from them.
#[test]
fn a_boxed_wide_value_carrying_nobox_fields_reads_and_writes_them() {
    let types = WIDE_UNIFORM_SRC.replace("fn main() { print(\"{queue_sprites(0.5, 3)}\") }", "");
    let src = format!(
        "{types}
fn anchor_of(u: RelightU) -> P4 {{ return u.mesh_info }}
fn with_anchor(u: RelightU, a: P4) -> RelightU {{ return RelightU {{ ..u, mesh_info: a }} }}
fn axis_sum(u: RelightU) -> float {{
    let ax = u.axis_x
    let ay = u.axis_y
    return ax.x + ax.y + ax.z + ay.x + ay.y + ay.z
}}
fn roundtrip(seed: float) -> float {{
    let u = with_anchor(relight_base(seed), P4 {{ x: 2.0, y: 3.0, z: 4.0, w: 5.0 }})
    let a = anchor_of(u)
    return a.x + a.y + a.z + a.w + axis_sum(u)
}}
fn main() {{ print(\"{{roundtrip(0.0)}}\") }}
"
    );
    let lua = try_emit_module(&src).expect("reading nobox fields out of a boxed wide value");
    assert_eq!(run_main(&lua), "16");
    // The `nobox` value that IS a parameter keeps its scalar lanes.
    assert_eq!(
        emitted_params(&lua, "with_anchor").len(),
        5,
        "a nobox P4 parameter must stay four lanes beside the one boxed slot:\n{}",
        function_body(&lua, "with_anchor")
    );
}

/// The retained witness of the OLD ABI must keep failing; a compiler fix
/// produces different Lua rather than making that file load.
#[test]
fn the_retained_old_abi_witness_still_fails_to_load() {
    let witness = include_str!("fixtures/generated-function.lua");
    let err = luajit_loads(witness)
        .expect_err("the retained witness is a diagnostic of the old ABI, not a repaired oracle");
    assert!(err.contains("too complex"), "unexpected parse error: {err}");
}

// --- gate 2: the cut, exactly ---------------------------------------------

/// `payload` float leaves in an ordinary value, crossed at a parameter and at a
/// return; `nullable` makes the parameter and return `T?`.
fn cut_fixture(payload: usize, nullable: bool) -> String {
    let q = if nullable { "?" } else { "" };
    let read = if nullable {
        "if u == null { return -1.0 }\n    return u.f0"
    } else {
        "return u.f0"
    };
    format!(
        "value V {{ {fields} }}\n\
         fn mk(x: float) -> V {{ return V {{ {inits} }} }}\n\
         fn thru(u: V{q}) -> V{q} {{ return u }}\n\
         fn read(u: V{q}) -> float {{ {read} }}\n\
         fn main() {{ print(\"{{read(thru(mk(1.0)))}}\") }}\n",
        fields = float_fields(payload),
        inits = float_inits(payload, "x"),
    )
}

#[test]
fn a_value_at_the_cut_keeps_the_scalar_wire() {
    let lua = try_emit_module(&cut_fixture(WIDE_VALUE_PAYLOAD_LEAF_CUT, false)).expect("emits");
    assert_eq!(
        emitted_params(&lua, "thru").len(),
        WIDE_VALUE_PAYLOAD_LEAF_CUT,
        "a value AT the cut must stay scalar:\n{}",
        function_body(&lua, "thru")
    );
}

#[test]
fn a_value_one_leaf_over_the_cut_takes_the_aggregate_wire() {
    let lua = try_emit_module(&cut_fixture(WIDE_VALUE_PAYLOAD_LEAF_CUT + 1, false)).expect("emits");
    assert_eq!(
        emitted_params(&lua, "thru").len(),
        1,
        "a value one leaf over the cut must cross as one slot:\n{}",
        function_body(&lua, "thru")
    );
}

/// `K` compares the PAYLOAD leaf count, not `ValueShape::leaf_count()`, which
/// also counts the nullable presence flag, so a nullable ORDINARY `value`
/// takes the same wire as its payload: a presence lane plus its leaves at or
/// under the cut, one boxed slot over it.
#[test]
fn a_nullable_ordinary_value_takes_the_cut_of_its_payload() {
    for (payload, lanes) in [(2, 3), (WIDE_VALUE_PAYLOAD_LEAF_CUT + 1, 1)] {
        let lua = try_emit_module(&cut_fixture(payload, true)).expect("emits");
        assert_eq!(
            emitted_params(&lua, "thru").len(),
            lanes,
            "`V?` at {payload} payload leaves:\n{}",
            function_body(&lua, "thru")
        );
    }
    for payload in [
        2,
        WIDE_VALUE_PAYLOAD_LEAF_CUT,
        WIDE_VALUE_PAYLOAD_LEAF_CUT + 1,
    ] {
        assert_eq!(
            run_main(&try_emit_module(&cut_fixture(payload, true)).expect("emits")),
            "1",
        );
    }
}

/// A `nobox` value is never boxed, at any width.
#[test]
fn a_wide_nobox_value_keeps_its_scalar_lanes() {
    let payload = WIDE_VALUE_PAYLOAD_LEAF_CUT + 1;
    let src = format!(
        "nobox value N {{ {fields} }}\n\
         fn mk(x: float) -> N {{ return N {{ {inits} }} }}\n\
         fn thru(u: N) -> N {{ return u }}\n\
         fn read(u: N) -> float {{ return u.f0 }}\n\
         fn main() {{ print(\"{{read(thru(mk(1.0)))}}\") }}\n",
        fields = float_fields(payload),
        inits = float_inits(payload, "x"),
    );
    let lua = try_emit_module(&src).expect("emits");
    assert_eq!(
        emitted_params(&lua, "thru").len(),
        payload,
        "a nobox value wider than the cut must keep its lanes:\n{}",
        function_body(&lua, "thru")
    );
    assert_eq!(run_main(&lua), "1");
}

// --- gate 2: the per-signature lane budget --------------------------------

/// The budget fixtures' member width. Every member must stay UNDER the cut, so
/// the per-value rule cannot fire and the budget is the only actor; and five of
/// them must be over the budget, with one boxed still over and two boxed under,
/// so "widest first, until it fits" has a distinguishable answer. The
/// preconditions are ASSERTED below rather than trusted, so a `K` or budget
/// that moves this fixture out of its regime fails loudly instead of quietly
/// testing nothing.
const BUDGET_MEMBER: usize = 24;
/// The `nobox` member of `budget_nobox_widest`, wider than `BUDGET_MEMBER` so
/// that a budget ranking candidates by lanes alone would reach for it first.
const BUDGET_NOBOX_WIDE: usize = 30;
/// The narrow members of `budget_mixed`.
const BUDGET_NARROW: usize = 12;

fn budget_src() -> String {
    // Whole-value bindings retain the declared transport for the budget test.
    let value = |kw: &str, name: &str, n: usize| {
        let fields = (0..n)
            .map(|k| format!("m{k}: float"))
            .collect::<Vec<_>>()
            .join(", ");
        let inits = (0..n)
            .map(|k| format!("m{k}: x"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{kw} value {name} {{ {fields} }}\nfn mk{name}(x: float) -> {name} {{ return {name} {{ {inits} }} }}\n"
        )
    };
    format!(
        "{}{}{}{}
fn budget_five(a_value: M, b_value: M, c_value: M, d_value: M, e_value: M) -> float {{
    let a = a_value; let b = b_value; let c = c_value; let d = d_value; let e = e_value;
    return a.m0 + b.m1 + c.m2 + d.m3 + e.m4
}}

fn budget_mixed(a_value: M, b_value: M, c_value: M, n_value: NB, d_value: N) -> float {{
    let a = a_value; let b = b_value; let c = c_value; let n = n_value; let d = d_value;
    return a.m0 + b.m1 + c.m2 + n.m0 + d.m3
}}

// The `nobox` member is the WIDEST thing in this signature, so a budget that
// ranked candidates by lanes without excluding `nobox` would box it first.
fn budget_nobox_widest(n_value: NW, a_value: M, b_value: M, c_value: M) -> float {{
    let n = n_value; let a = a_value; let b = b_value; let c = c_value;
    return n.m0 + a.m0 + b.m1 + c.m2
}}

fn main() {{
    print(\"{{budget_five(mkM(1.0), mkM(2.0), mkM(3.0), mkM(4.0), mkM(5.0))}}\")
}}
",
        value("", "M", BUDGET_MEMBER),
        value("", "N", BUDGET_NARROW),
        value("nobox", "NB", BUDGET_NARROW),
        value("nobox", "NW", BUDGET_NOBOX_WIDE),
    )
}

/// The regime the budget fixtures need. Checked as its own test so a `K` or
/// budget change that invalidates them says so directly.
#[test]
#[expect(
    clippy::assertions_on_constants,
    reason = "Keep fixture assumptions checked by this named runtime test"
)]
fn the_budget_fixtures_are_in_the_regime_they_claim() {
    let b = SIGNATURE_SCALAR_LANE_BUDGET;
    assert!(
        BUDGET_MEMBER <= WIDE_VALUE_PAYLOAD_LEAF_CUT
            && BUDGET_NOBOX_WIDE <= WIDE_VALUE_PAYLOAD_LEAF_CUT
            && BUDGET_NARROW <= WIDE_VALUE_PAYLOAD_LEAF_CUT,
        "a member wider than the cut would be boxed by the CUT, not the budget"
    );
    assert!(
        5 * BUDGET_MEMBER + 1 > b,
        "five members must exceed the budget"
    );
    assert!(
        1 + 4 * BUDGET_MEMBER + 1 > b,
        "boxing one member must not be enough"
    );
    assert!(
        2 + 3 * BUDGET_MEMBER < b,
        "boxing two members must be enough"
    );
    assert!(
        (1 + 2 * BUDGET_MEMBER + BUDGET_NARROW * 2 + 1..3 * BUDGET_MEMBER + BUDGET_NARROW * 2 + 1)
            .contains(&b),
        "budget_mixed must need exactly one box"
    );
    assert!(
        (BUDGET_NOBOX_WIDE + 1 + 2 * BUDGET_MEMBER + 1..BUDGET_NOBOX_WIDE + 3 * BUDGET_MEMBER + 1)
            .contains(&b),
        "budget_nobox_widest must need exactly one box"
    );
}

#[test]
fn a_signature_over_the_lane_budget_boxes_widest_first() {
    let lua = try_emit_module(&budget_src()).expect("emits");
    let params = emitted_params(&lua, "budget_five");
    assert_eq!(
        params.len(),
        2 + BUDGET_MEMBER * 3,
        "the budget must box the two widest values and leave the rest scalar:\n{}",
        function_body(&lua, "budget_five")
    );
    assert!(
        params.len() < SIGNATURE_SCALAR_LANE_BUDGET,
        "the surviving lanes must fit the budget: {}",
        params.len() + 1
    );
    assert_eq!(
        run_main(&lua),
        "15",
        "boxing must not change what the call computes"
    );
}

/// The budget never boxes a `nobox` member, and counts its REAL lanes
/// (presence flags included) against the budget like any other.
#[test]
fn the_budget_never_boxes_a_nobox_member() {
    let lua = try_emit_module(&budget_src()).expect("emits");
    let params = emitted_params(&lua, "budget_mixed");
    assert_eq!(
        params.len(),
        1 + BUDGET_MEMBER * 2 + BUDGET_NARROW * 2,
        "exactly one ordinary value must box, and the nobox member must keep its lanes:\n{}",
        function_body(&lua, "budget_mixed")
    );
    // The WIDEST member here is the `nobox` one. Ranking candidates without
    // excluding `nobox` would box it and stop; the budget must box an `M`.
    let widest = emitted_params(&lua, "budget_nobox_widest");
    assert_eq!(
        widest.len(),
        BUDGET_NOBOX_WIDE + 1 + BUDGET_MEMBER * 2,
        "the budget boxed the nobox member instead of the widest ORDINARY value:\n{}",
        function_body(&lua, "budget_nobox_widest")
    );
}

// --- the wire is a property of the TYPE, so every call kind agrees ---------

/// `value W` of one leaf MORE than the cut, with a constructor that puts `x` in
/// `a0` and zero everywhere else. Derived from the constant so the fixture
/// tracks the cut instead of pinning a width the cut can outgrow.
fn over_cut_value() -> String {
    let n = WIDE_VALUE_PAYLOAD_LEAF_CUT + 1;
    let fields = (0..n)
        .map(|k| format!("a{k}: float"))
        .collect::<Vec<_>>()
        .join(", ");
    let inits = std::iter::once("a0: x".to_string())
        .chain((1..n).map(|k| format!("a{k}: 0.0")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("value W {{ {fields} }}\nfn mkw(x: float) -> W {{ return W {{ {inits} }} }}\n")
}

/// Direct calls, calls through a function value, closures, methods/UFCS,
/// generic instantiations and a `co fn` all target the same boxed wire. Every
/// one of them runs; a disagreement is a Lua arity mismatch, which shows up as
/// a nil arithmetic error rather than a wrong number.
const CALL_KINDS_BODY: &str = r#"
fn bump(w: W) -> W { return W { ..w, a0: w.a0 + 1.0 } }
fn head(w: W) -> float { return w.a0 }

fn head_method(w: W) -> float { return w.a0 * 10.0 }

fn thru_generic<T>(t: T) -> T { return t }

fn apply(f: fn(W) -> W, w: W) -> W { return f(w) }

fn via_closure(w: W) -> W {
    let step = (v: W) => bump(v)
    return step(w)
}

co fn pipeline(w: W) -> Coroutine<W, float> {
    let a = bump(w)
    yield a
    let b = bump(a)
    return head(b)
}

fn two_wide(w: W) -> (W, W) { return (bump(w), bump(bump(w))) }

fn tail_forward(w: W) -> W { return bump(w) }

fn main() {
    let w = mkw(0.0)
    let direct = head(bump(w))
    let indirect = head(apply(bump, w))
    let closed = head(via_closure(w))
    let generic = head(thru_generic(bump(w)))
    let method = w.head_method()
    let tailed = head(tail_forward(w))
    let (p, q) = two_wide(w)
    let multi = head(p) + head(q)
    let co = pipeline(w)
    let first = co.resume()
    let yielded = if first == null { -1.0 } else { head(first) }
    let _ = co.resume()
    let finished = co.result() ?? -1.0
    print("{direct} {indirect} {closed} {generic} {method} {tailed} {multi} {yielded} {finished}")
}
"#;

fn call_kinds_src() -> String {
    format!("{}{CALL_KINDS_BODY}", over_cut_value())
}

#[test]
fn every_call_kind_agrees_on_the_boxed_wire() {
    let lua = try_emit_module(&call_kinds_src()).expect("emits");
    assert_eq!(
        emitted_params(&lua, "bump").len(),
        1,
        "a value one leaf over the cut must box:\n{}",
        function_body(&lua, "bump")
    );
    assert_eq!(run_main(&lua), "1 1 1 1 0 1 3 1 2",);
}

/// Value semantics survive the box: a callee that mutates its own copy of a
/// boxed parameter must not be observable in the caller, and vice versa.
#[test]
fn a_boxed_parameter_does_not_alias_the_callers_value() {
    let src = format!(
        "{}
global CALLER: W
global CALLEE: W
// The callee keeps its boxed parameter in its own storage and then mutates it.
// If the box were the caller's, `CALLER.a0` would move with it.
fn keep_and_mutate(w: W) -> float {{
    CALLEE = w
    CALLEE.a0 = 99.0
    return CALLEE.a0
}}
// And the other direction: the caller writes AFTER the call, which the copy the
// callee kept must not see.
fn main() {{
    CALLER = mkw(1.0)
    let seen = keep_and_mutate(CALLER)
    let caller_after_callee_wrote = CALLER.a0
    CALLER.a0 = 7.0
    print(\"{{caller_after_callee_wrote}} {{seen}} {{CALLER.a0}} {{CALLEE.a0}}\")
}}
",
        over_cut_value()
    );
    let lua = try_emit_module(&src).expect("emits");
    assert_eq!(
        emitted_params(&lua, "keep_and_mutate").len(),
        1,
        "the fixture must exercise a BOXED parameter:\n{}",
        function_body(&lua, "keep_and_mutate")
    );
    assert_eq!(
        run_main(&lua),
        "1 99 7 99",
        "a boxed parameter must not alias the caller's value"
    );
}

/// A callee that returns its boxed parameter returns a COPY. The parameter box
/// is the caller's (borrowed, never written), and a caller may store the result
/// into a place and write through it, which must not reach the argument. The
/// forwarder lends its immutable argument; the mutable store owns the copy.
#[test]
fn a_forwarding_callee_lends_the_box_until_a_mutable_store() {
    let src = format!(
        "{}struct HoldW {{ w: W }}
fn forward(w: W) -> W {{ return w }}
fn main() {{
    let w = mkw(3.0)
    let h = HoldW {{ w: forward(w) }}
    h.w.a0 = 9.0
    print(\"{{w.a0}} {{h.w.a0}}\")
}}
",
        over_cut_value()
    );
    let lua = try_emit_module(&src).expect("emits");
    let body = function_body(&lua, "forward");
    assert_eq!(
        emitted_params(&lua, "forward").len(),
        1,
        "the forwarder must take the box:\n{body}"
    );
    assert_eq!(
        body.matches("_ct(").count(),
        0,
        "the forwarder must reuse the argument box:\n{body}"
    );
    assert_eq!(run_main(&lua), "3 9");
}

/// The boxed wire is a property of the type alone, so a body whose CALLEE's
/// wire changed must be re-emitted. Byte identity between a full build and an
/// incremental one is what proves both agree; the witness here is narrower and
/// local: every definition the full build emits is emitted with the same
/// parameter arity for the boxed callee and its callers.
#[test]
fn the_boxed_wire_is_consistent_across_every_emitted_definition() {
    let lua = try_emit_module(&call_kinds_src()).expect("emits");
    let names = definition_names(&lua);
    for name in names {
        if !name.starts_with("bump") {
            continue;
        }
        assert_eq!(
            emitted_params(&lua, name).len(),
            1,
            "a `bump` variant disagrees on the boxed wire:\n{}",
            function_body(&lua, name)
        );
    }
}

/// The frame-limit diagnostic is what a wide `nobox` value still meets, and it
/// still names the value: the ABI cut must not have turned a `nobox` rejection
/// into a silent box.
#[test]
fn a_frame_limit_rejection_still_names_the_nobox_value() {
    let fields = float_fields(50);
    let inits = float_inits(50, "x");
    let copies: Vec<String> = (1..50).map(|k| format!("f{k}: w.f{k}")).collect();
    let mut src = format!(
        "nobox value W {{ {fields} }}\n\
         fn mk(x: float) -> W {{ return W {{ {inits} }} }}\n\
         fn tweak(w: W) -> W {{ return W {{ f0: w.f0 + 1.0, {} }} }}\n\
         fn check(value: W, k: int) -> float {{ let w = value; return w.f0 + w.f49 + float(k) }}\n\
         fn run(x: float) -> float {{\n    let v0 = mk(x)\n",
        copies.join(", "),
    );
    for i in 1..4 {
        src.push_str(&format!("    let v{i} = tweak(v{})\n", i - 1));
    }
    src.push_str("    let total = 0.0\n");
    for i in 0..4 {
        src.push_str(&format!("    total += check(v{i}, {i})\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    let err = try_emit_module(&src).expect_err("four live 50-leaf nobox values must not emit");
    assert!(
        err.message.contains("nobox"),
        "no nobox contributor:\n{}",
        err.message
    );
    let lua = emit_module_without_frame_limits(&src);
    luajit_loads(&lua).expect_err("must not load");
}

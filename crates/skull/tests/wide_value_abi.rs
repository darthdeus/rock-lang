use skull_mir::lower::{SIGNATURE_SCALAR_LANE_BUDGET, WIDE_VALUE_PAYLOAD_LEAF_CUT};

use crate::common::{
    definition_names, emit_module_without_frame_limits, function_body, function_params,
    luajit_loads, run_main, try_emit_module,
};

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
fn the_wide_uniform_witness_uses_one_slot_per_wide_value() {
    let lua = try_emit_module(WIDE_UNIFORM_SRC).expect("the wide-uniform shape must emit");
    luajit_loads(&lua).expect("the emitted chunk must load");
    assert_eq!(function_params(&lua, "sprite_uniform").len(), 2);
    assert_eq!(function_params(&lua, "uniform_signal").len(), 1);
    assert_eq!(run_main(&lua), "19.5");
}

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
    assert_eq!(function_params(&lua, "with_anchor").len(), 5);
}

#[test]
fn the_retained_old_abi_witness_still_fails_to_load() {
    let witness = include_str!("fixtures/generated-function.lua");
    let err = luajit_loads(witness)
        .expect_err("the retained witness is a diagnostic of the old ABI, not a repaired oracle");
    assert!(err.contains("too complex"));
}

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
        function_params(&lua, "thru").len(),
        WIDE_VALUE_PAYLOAD_LEAF_CUT
    );
}

#[test]
fn a_value_one_leaf_over_the_cut_takes_the_aggregate_wire() {
    let lua = try_emit_module(&cut_fixture(WIDE_VALUE_PAYLOAD_LEAF_CUT + 1, false)).expect("emits");
    assert_eq!(function_params(&lua, "thru").len(), 1);
}

#[test]
fn a_nullable_ordinary_value_takes_the_cut_of_its_payload() {
    for (payload, lanes) in [
        (2, Some(3)),
        (WIDE_VALUE_PAYLOAD_LEAF_CUT, None),
        (WIDE_VALUE_PAYLOAD_LEAF_CUT + 1, Some(1)),
    ] {
        let src = cut_fixture(payload, true);
        let lua = try_emit_module(&src).expect("emits");
        if let Some(lanes) = lanes {
            assert_eq!(function_params(&lua, "thru").len(), lanes);
        }
        assert_eq!(run_main(&lua), "1");
    }
}

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
    assert_eq!(function_params(&lua, "thru").len(), payload);
    assert_eq!(run_main(&lua), "1");
}

const BUDGET_MEMBER: usize = 24;
const BUDGET_NOBOX_WIDE: usize = 30;
const BUDGET_NARROW: usize = 12;

fn budget_src() -> String {
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

#[test]
#[expect(
    clippy::assertions_on_constants,
    reason = "Keep fixture assumptions checked by this named runtime test"
)]
fn the_lane_budget_boxes_widest_ordinary_values_and_never_nobox_values() {
    let b = SIGNATURE_SCALAR_LANE_BUDGET;
    assert!(
        BUDGET_MEMBER <= WIDE_VALUE_PAYLOAD_LEAF_CUT
            && BUDGET_NOBOX_WIDE <= WIDE_VALUE_PAYLOAD_LEAF_CUT
            && BUDGET_NARROW <= WIDE_VALUE_PAYLOAD_LEAF_CUT
    );
    assert!(5 * BUDGET_MEMBER + 1 > b);
    assert!(1 + 4 * BUDGET_MEMBER + 1 > b);
    assert!(2 + 3 * BUDGET_MEMBER < b);
    assert!(
        (1 + 2 * BUDGET_MEMBER + BUDGET_NARROW * 2 + 1..3 * BUDGET_MEMBER + BUDGET_NARROW * 2 + 1)
            .contains(&b)
    );
    assert!(
        (BUDGET_NOBOX_WIDE + 1 + 2 * BUDGET_MEMBER + 1..BUDGET_NOBOX_WIDE + 3 * BUDGET_MEMBER + 1)
            .contains(&b)
    );
    let src = budget_src();
    let lua = try_emit_module(&src).expect("emits");
    let params = function_params(&lua, "budget_five");
    assert_eq!(params.len(), 2 + BUDGET_MEMBER * 3);
    assert!(params.len() < SIGNATURE_SCALAR_LANE_BUDGET);
    assert_eq!(run_main(&lua), "15");
    let params = function_params(&lua, "budget_mixed");
    assert_eq!(params.len(), 1 + BUDGET_MEMBER * 2 + BUDGET_NARROW * 2);
    let widest = function_params(&lua, "budget_nobox_widest");
    assert_eq!(widest.len(), BUDGET_NOBOX_WIDE + 1 + BUDGET_MEMBER * 2);
}

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
    let src = call_kinds_src();
    let lua = try_emit_module(&src).expect("emits");
    assert_eq!(function_params(&lua, "bump").len(), 1);
    let names = definition_names(&lua);
    for name in names {
        if !name.starts_with("bump") {
            continue;
        }
        assert_eq!(function_params(&lua, name).len(), 1);
    }
    assert_eq!(run_main(&lua), "1 1 1 1 0 1 3 1 2");
}

#[test]
fn a_boxed_parameter_does_not_alias_the_callers_value() {
    let src = format!(
        "{}
global CALLER: W
global CALLEE: W
fn keep_and_mutate(w: W) -> float {{
    CALLEE = w
    CALLEE.a0 = 99.0
    return CALLEE.a0
}}
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
    assert_eq!(function_params(&lua, "keep_and_mutate").len(), 1);
    assert_eq!(run_main(&lua), "1 99 7 99");
}

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
    assert_eq!(function_params(&lua, "forward").len(), 1);
    assert_eq!(body.matches("_ct(").count(), 0);
    assert_eq!(run_main(&lua), "3 9");
}

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
    assert!(err.message.contains("nobox"));
    let lua = emit_module_without_frame_limits(&src);
    luajit_loads(&lua).expect_err("must not load");
}

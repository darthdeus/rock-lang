use crate::common::{
    body_id, check_incremental, definition_names, definitions_matching, edit, emit_full_of,
    function_body, global_body, run_luajit, ws_of, ws_with_stdlib,
};
use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::test_support::emit_incremental;
use skull::mir_emit::{MirEmit, emit_full, emit_overlay as emit_overlay_result};

fn emit_overlay(
    ws: &Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> MirEmit {
    emit_overlay_result(ws, cache, reinferred).expect("emit_overlay")
}

fn check_edit(build: fn(&str) -> Workspace, before: &str, after: &str, edited: &str) {
    let (_, mut cache) = emit_full_of(&build(before), "initial emit");
    let ws = build(after);
    check_incremental(&ws, &mut cache, &[body_id(&ws, edited)]);
}

#[test]
fn editing_a_shadowed_backend_body_does_not_publish_its_function_values() {
    let src = r#"
fn other() -> int { 4 }
fn probe() -> int { 3 }
inline(lua) fn probe() -> int = "return 7"
fn main() { print(probe()) }
"#;
    let before = ws_with_stdlib(src);
    let (_, mut cache) = emit_full_of(&before, "initial emit");
    let after = ws_with_stdlib(&src.replace("{ 3 }", "{ let f: () -> int = other; f() }"));
    let edited = [after
        .modules
        .iter()
        .find_map(|module| {
            module.functions.iter().find_map(|(id, function)| {
                let owner = BodyOwner::Function(id);
                (crate::common::name_matches(function.name, "probe")
                    && module.bodies.contains_key(&owner))
                .then_some((module.id, owner))
            })
        })
        .expect("Rock body of the backend family")];
    let updated = emit_incremental(&after, &mut cache, &edited).unwrap();
    let (fresh, fresh_cache) = emit_full_of(&after, "fresh emit");
    assert_eq!(
        cache.lua_view().fn_value_targets,
        fresh_cache.lua_view().fn_value_targets
    );
    assert_eq!(updated.lua, fresh.lua);
}

#[test]
fn scalar_constant_edits_refresh_full_incremental_and_overlay_emission() {
    let v1 = "const BASE: float = 1.5\nconst ALIAS: float = BASE\n@nomangle fn choose() -> float { ALIAS }";
    let v2 = v1.replace("1.5", "-0.0");
    let ws2 = ws_of(&v2);
    let edit = edit(&ws_of(v1), &ws2, &[global_body(&ws2, "BASE")]);
    assert!(function_body(&edit.base.lua, "choose").contains("1.5"));
    let overlay = edit.overlay.expect("overlay");
    let body = function_body(&overlay.lua, "choose");
    assert!(body.contains("-0.0") && !body.contains("ALIAS"));
}

#[test]
fn derived_scalar_constant_edits_refresh_transitive_readers_at_runtime() {
    let template = "fn source() -> int { 32 }\nconst BASE: int = INIT\nconst SCALE: int = BASE * 128\nconst DERIVED: int = SCALE + 1\n@nomangle fn choose() -> int { DERIVED }";
    assert_eq!(template.matches("INIT").count(), 1);
    let with_init = |init: &str| template.replacen("INIT", init, 1);
    let ws1 = ws_with_stdlib(&with_init("8"));
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut incremental_cache) = emit_full(&ws1, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&ws1, mid).unwrap();
    let mut runtime = format!("{}\nassert(choose() == 1025)\n", full.lua);
    for (label, init, expected_before, expected_after, folded_after) in [
        ("BASE 8 -> 16", "16", 1025, 2049, true),
        ("BASE 16 -> source()", "source()", 2049, 4097, false),
        ("BASE source() -> 8", "8", 4097, 1025, true),
    ] {
        let source = with_init(init);
        let ws2 = ws_with_stdlib(&source);
        let edited = [global_body(&ws2, "BASE")];
        check_incremental(&ws2, &mut incremental_cache, &edited);
        let overlay = emit_overlay(&ws2, &mut overlay_cache, &edited);
        let after = function_body(&overlay.lua, "choose");
        assert_eq!(
            after.contains(&expected_after.to_string()),
            folded_after,
            "{label}"
        );
        assert_eq!(after.contains("DERIVED"), !folded_after);
        runtime.push_str(&format!(
            "assert(choose() == {expected_before})\n{}\nassert(choose() == {expected_after})\n",
            overlay.lua
        ));
    }
    runtime.push_str("print('ok')");
    assert_eq!(run_luajit(&runtime), "ok");
}

fn inline_budget_eligibility_flip_fixture(large_helper: bool) -> String {
    let mut src = String::from("@inline @nomangle fn helper(flag: bool) -> int {\n");
    if large_helper {
        for i in 0..64 {
            src.push_str(&format!(
                "let value_{i} = {i}\nif flag {{ return value_{i} }}\n"
            ));
        }
        src.push_str("return 17\n}\n");
    } else {
        src.push_str("if flag { return 3 }\nreturn 5\n}\n");
    }
    src.push_str("@nomangle fn caller() -> int {\n");
    for i in 0..380 {
        src.push_str(&format!("let unused_{i} = 0\nunused_{i} = {i}\n"));
    }
    src.push_str("return helper(false)\n}\n@nomangle fn other() -> int { 7 }\n");
    src
}

#[test]
fn transitive_effect_changes_rebuild_residual_callers_in_both_directions() {
    let grow = r#"
fn leaf(xs: [int]) { }
fn middle(xs: [int]) { leaf(xs) }
fn hot(xs: [int], i: int) -> int {
    if i >= 0 && i < xs.len {
        middle(xs)
        return xs[i]
    }
    return -1
}
fn main() {
    let xs: [int] = [7]
    print("{hot(xs, 0)}")
}
"#;
    let shrink = grow.replace(
        "fn leaf(xs: [int]) { }",
        "fn leaf(xs: [int]) { xs.len = 0 }",
    );
    let checked = |lua: &str| {
        let hot = function_body(lua, "hot");
        hot.contains("__vec_checked_get(") || hot.contains("__vec_checked_index(")
    };
    let ws1 = ws_with_stdlib(grow);
    let (full1, mut incremental_cache) = emit_full_of(&ws1, "emit_full v1");
    let (_, mut overlay_cache) = emit_full_of(&ws1, "overlay base v1");
    assert!(!checked(&full1.lua));
    for (_step, src, expect_check) in [("shrink", shrink.as_str(), true), ("grow", grow, false)] {
        let ws = ws_with_stdlib(src);
        let reinferred = [body_id(&ws, "leaf")];
        let incremental = check_incremental(&ws, &mut incremental_cache, &reinferred);
        assert_eq!(checked(&incremental.lua), expect_check);
        let overlay = emit_overlay(&ws, &mut overlay_cache, &reinferred);
        assert_eq!(checked(&overlay.lua), expect_check);
    }
}

fn tally_workspace(marker: &str, use_q: bool) -> Workspace {
    let result = if use_q {
        "tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a"
    } else {
        "G1"
    };
    ws_with_stdlib(&format!(
        r#"
value P {{ x: float, y: float }}
value Q {{ a: float, b: float, c: float }}
fn tally_of<T>(xs: [T]) -> T {{ print("{marker}") return xs[0] }}
global G1: float = tally_of([P {{ x: 1.0, y: 0.0 }}]).x
fn use_it() -> float {{ return {result} }}
"#
    ))
}

#[test]
fn specialization_overlays_track_added_pruned_and_restored_instances() {
    for (starts_with_q, edits) in [
        (
            false,
            &[
                (true, "MARK1", "use_it", None, true),
                (true, "MARK2", "tally_of", Some(2), true),
            ][..],
        ),
        (
            true,
            &[
                (false, "MARK1", "use_it", None, false),
                (false, "MARK2", "tally_of", None, false),
                (true, "MARK2", "use_it", None, true),
            ][..],
        ),
    ] {
        let (full, mut cache) =
            emit_full_of(&tally_workspace("MARK1", starts_with_q), "initial emit");
        let tallies = |lua: &str| {
            definition_names(lua)
                .into_iter()
                .filter(|name| name.starts_with("tally_of_"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let initial = tallies(&full.lua);
        assert!(initial.iter().any(|name| name.contains("P")));
        assert_eq!(
            initial.iter().filter(|name| name.contains("Q")).count(),
            usize::from(starts_with_q)
        );
        for &(use_q, marker, edited, count, emits_q) in edits {
            let ws = tally_workspace(marker, use_q);
            let overlay = emit_overlay(&ws, &mut cache, &[body_id(&ws, edited)]);
            let bodies = tallies(&overlay.lua);
            if let Some(count) = count {
                assert_eq!(bodies.len(), count, "{edited}, use_q={use_q}");
            }
            assert_eq!(
                bodies.iter().filter(|name| name.contains("Q")).count(),
                usize::from(emits_q)
            );
            for body in bodies {
                if count.is_some() || body.contains("Q") {
                    assert!(
                        function_body(&overlay.lua, &body).contains(marker),
                        "{body}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_standalone_bounded_body_survives_an_incremental_rebuild() {
    let _inline = crate::common::inline_off_guard();
    let v1 = "fn total<I: Iterator<int>>(it: I) -> int {\n\
              \x20   let s = 0\n\
              \x20   for v in it { s = s + v }\n\
              \x20   return s\n\
              }\n\
              fn use_it() -> int {\n\
              \x20   let xs: [int] = [1, 2, 3]\n\
              \x20   return total(xs.iter())\n\
              }";
    let v2 = v1.replace("let s = 0", "let s = 100");
    let standalone = |lua: &str| {
        definitions_matching(lua, "_spec_")
            .into_iter()
            .filter(|name| name.starts_with("total_"))
            .count()
    };
    let (full, mut cache) = emit_full_of(&ws_with_stdlib(v1), "initial emit");
    assert_eq!(standalone(&full.lua), 1);
    let ws = ws_with_stdlib(&v2);
    let incremental = check_incremental(&ws, &mut cache, &[body_id(&ws, "total")]);
    assert_eq!(standalone(&incremental.lua), 1);
}

#[test]
fn coroutine_frame_nil_stores_share_full_incremental_and_overlay_emission() {
    let v1 = r#"
enum FrameYield { Empty, Pair(int, int) }
co fn stream() -> Coroutine<FrameYield> {
    yield FrameYield::Pair(1, 2)
    yield FrameYield::Empty
}
inline(lua) fn slots_empty(c: Coroutine<FrameYield>) -> bool = """
    return c.__y1 == nil and c.__y2 == nil
"""
@nomangle fn sample(expected: int) -> bool {
    let c = stream()
    let first = c.resume() == FrameYield::Pair(expected, 2)
    let second = c.resume() == FrameYield::Empty
    return first && second && slots_empty(c)
}
"#;
    let v2 = v1.replace("Pair(1, 2)", "Pair(3, 2)");
    let ws2 = ws_with_stdlib(&v2);
    let edit = edit(&ws_with_stdlib(v1), &ws2, &[body_id(&ws2, "stream")]);
    let overlay = edit.overlay.expect("overlay");
    let step = "stream_0__step";
    assert_eq!(
        function_body(&overlay.lua, step),
        function_body(&edit.incremental.lua, step)
    );
    let run = format!(
        "{}\nprint(sample(1))\n{}\nprint(sample(3))\n",
        edit.base.lua, overlay.lua
    );
    assert_eq!(run_luajit(&run), "true\ntrue");
}

const SHARED_ABI_SLOTS: &str = r#"
@lifecycle(phase="update", owner="process")
deco init tick {}
global TICKS: int = 0
@nomangle fn ticks() -> int { TICKS }
@tick fn first() {}
@tick fn second() {}
@tick fn third() {}
fn main() {}
"#;

#[test]
fn shared_call_abis_are_staged_once_and_remain_compatible_across_overlays() {
    let ws1 = ws_of(SHARED_ABI_SLOTS);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full");
    let abis: std::collections::BTreeSet<&str> = full
        .code_artifact
        .entries
        .values()
        .filter(|entry| entry.escaping)
        .map(|entry| entry.compatibility.call_abi.as_str())
        .collect();
    let staged = full
        .lua
        .lines()
        .filter(|line| line.contains("__rock_code_slot_stage("))
        .count();
    assert!(staged > abis.len());
    for abi in abis {
        let literal = skull_codegen_lua::lua_string(abi);
        assert_eq!(full.lua.matches(literal.as_str()).count(), 1);
    }
    let v2 = SHARED_ABI_SLOTS.replace("@tick fn first() {}", "@tick fn first() { TICKS = 1 }");
    let escaping: Vec<_> = full
        .code_artifact
        .entries
        .values()
        .filter(|entry| entry.escaping)
        .collect();
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(overlay.lua.contains("__rock_code_slot_stage("));
    let run = format!(
        r#"{full}
local function snapshot()
  local rows = {{}}
  for key, slot in pairs(_G.__rock_code_slots) do rows[#rows + 1] = key .. "=" .. slot.abi end
  table.sort(rows)
  return table.concat(rows, "\n")
end
local before = snapshot()
{overlay}
assert(snapshot() == before, "the overlay changed a slot ABI")
__ROCK_LIFECYCLE_PLAN.update[1].func()
assert(ticks() == 1)
print(before)
"#,
        full = full.lua,
        overlay = overlay.lua
    );
    let out = run_luajit(&run);
    assert_eq!(out.lines().count(), escaping.len());
    for entry in escaping {
        let abi = format!("={}", entry.compatibility.call_abi);
        assert!(out.lines().any(|row| row.ends_with(&abi)));
    }
}

#[test]
fn fingerprint_includes_new_body_specializations() {
    let original =
        "fn identity<T>(value: T) -> T { return value }\nfn main() { let _ = identity(1) }";
    let changed = original.replace(
        "let _ = identity(1)",
        "let _ = identity(1)\nlet _ = identity(true)",
    );
    let ws1 = ws_of(original);
    let (baseline, mut cache) = emit_full_of(&ws1, "emit_full");
    let before = cache.artifact_fingerprint(&ws1).unwrap();
    let ws2 = ws_of(&changed);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "main")]);
    assert!(overlay.code_artifact.entries.len() > baseline.code_artifact.entries.len());
    assert_ne!(cache.artifact_fingerprint(&ws2).unwrap(), before);
}

#[test]
fn flat_ref_signature_change_refuses_an_overlay() {
    let v1 = r#"
value Cell { v: int }
value Outer { cell: Cell, w: int }
fn apply(c: ref Cell) -> int { c.v += 1
return c.v }
@nomangle fn tick() -> int {
    let xs: [Cell] = []
    xs.push(Cell { v: 10 })
    return apply(xs[0])
}
"#;
    let v2 = v1
        .replace("let xs: [Cell] = []", "let xs: [Outer] = []")
        .replace(
            "xs.push(Cell { v: 10 })",
            "xs.push(Outer { cell: Cell { v: 10 }, w: 20 })",
        )
        .replace("apply(xs[0])", "apply(xs[0].cell)");
    let ws1 = ws_with_stdlib(v1);
    let (_, mut cache) = emit_full_of(&ws1, "emit_full");
    let ws2 = ws_with_stdlib(&v2);
    let err = match emit_overlay_result(&ws2, &mut cache, &[body_id(&ws2, "tick")]) {
        Ok(_) => panic!("changing a flat variant's element/prefix must request restart"),
        Err(err) => err,
    };
    assert!(err.message.contains("CodeRef compatibility") && err.message.contains("safe restart"));
}

#[test]
fn newly_stringified_enum_gets_its_names_table_in_incremental_and_overlay() {
    let v1 = r#"
enum Mode { Idle, Busy }
@nomangle fn describe(m: Mode) -> string { return "mode" }
fn main() { print(describe(Mode::Busy)) }
"#;
    let v2 = v1.replace("return \"mode\"", "return \"{m}\"");
    let ws2 = ws_with_stdlib(&v2);
    let edit = edit(&ws_with_stdlib(v1), &ws2, &[body_id(&ws2, "describe")]);
    assert!(!edit.base.lua.contains("Mode::Busy"));
    let overlay = edit.overlay.expect("overlay");
    let run = format!("{}\n{}\nprint(describe(1))\n", edit.base.lua, overlay.lua);
    assert_eq!(run_luajit(&run), "Mode::Busy");
}

#[test]
fn body_edits_match_full_emission() {
    let enum_projection = r#"
enum Count { Zero, Seven }
value Item { spare: int = 11, count: Count = Count::Seven }
struct Holder { item: Item? }
@nomangle fn probe(holder: Holder) -> bool { holder.item?.count == Count::Zero }
fn main() { print(probe(Holder { item: Item {} })) }
"#;
    let indexed = r#"
value Config { chosen: int, spare: int, label: string }
@inline fn selected(items: [Config], i: int) -> int { items[i].chosen }
@nomangle fn caller(items: [Config], i: int) -> int { selected(items, i) }
"#;
    let leaf = "@inline\nfn helper() -> int { 3 }\n\
                fn use_it() -> float { helper() }\n\
                fn other() -> int { 7 }";
    let ctor = "value V2 { x: int, y: int }\n\
                @inline\nfn mk() -> V2 { V2 { x: 1, y: 2 } }\n\
                fn use_v() -> int { let v = mk()\n v.x }";
    let last_edge = "fn leaf<T>(x: T) -> T { x }\nfn main() { let x = leaf(3) }";
    let cases: [(fn(&str) -> Workspace, &str, String, &str); 6] = [
        (
            ws_with_stdlib,
            enum_projection,
            enum_projection.replace("== Count::Zero", "== Count::Seven"),
            "probe",
        ),
        (
            ws_with_stdlib,
            indexed,
            indexed.replace("items[i].chosen", "items[i].spare"),
            "selected",
        ),
        (ws_of, leaf, leaf.replace("{ 7 }", "{ 9 }"), "other"),
        (ws_of, ctor, ctor.replace("x: 1", "x: 9"), "mk"),
        (
            ws_with_stdlib,
            ABI_FLIP_V1,
            ABI_FLIP_V2.to_string(),
            "use_ref",
        ),
        (ws_of, last_edge, last_edge.replace("leaf(3)", "0"), "main"),
    ];
    for (build, v1, v2, edited) in cases {
        check_edit(build, v1, &v2, edited);
    }
}

#[test]
fn inline_decisions_follow_helper_edits() {
    let small = |value: i32| {
        format!(
            "@inline @nomangle fn helper() -> int {{ {value} }}\n\
             @nomangle fn caller() -> float {{ helper() }}\n\
             @nomangle fn other() -> int {{ 7 }}"
        )
    };
    let over_budget = |value: i32| {
        let mut src = format!(
            "@inline @nomangle fn helper() -> int {{ {value} }}\n\
             @nomangle fn caller() -> int {{\n"
        );
        for i in 0..401 {
            src.push_str(&format!("let unused_{i} = 0\nunused_{i} = {i}\n"));
        }
        src + "return helper()\n}\n@nomangle fn other() -> int { 7 }\n"
    };
    for (_label, v1, v2, residual_before, residual_after) in [
        ("stays inlined", small(3), small(5), false, false),
        ("stays residual", over_budget(3), over_budget(5), true, true),
        (
            "becomes inlinable",
            inline_budget_eligibility_flip_fixture(true),
            inline_budget_eligibility_flip_fixture(false),
            true,
            false,
        ),
    ] {
        let residual = |lua: &str| function_body(lua, "caller").contains("helper(");
        let (full, mut cache) = emit_full_of(&ws_of(&v1), "initial emit");
        assert_eq!(residual(&full.lua), residual_before);
        let ws = ws_of(&v2);
        let updated = check_incremental(&ws, &mut cache, &[body_id(&ws, "helper")]);
        assert_eq!(residual(&updated.lua), residual_after);
    }
}

const ABI_FLIP_V1: &str = "value V2 { x: float, y: float }\n\
    fn foo(v: V2) -> float { v.x }\n\
    fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n\
    fn use_ref() -> float { other() }";
const ABI_FLIP_V2: &str = "value V2 { x: float, y: float }\n\
    fn foo(v: V2) -> float { v.x }\n\
    fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n\
    fn use_ref() -> float {\n\
    let f = foo\n\
    f(V2 { x: 1.0, y: 2.0 })\
    }";

#[test]
fn a_failed_graph_added_body_is_rejected_by_every_emit_route() {
    let common = "nobox value NV2 { x: float, y: float }\n\
                  fn bad<T>(xs: [T]) -> int {\n\
                  \x20   let t = (xs[0], 1.0)\n\
                  \x20   print(\"{t}\")\n\
                  \x20   return 0\n\
                  }\n";
    let v1 = common.to_string() + "fn use_it() -> int { return 0 }";
    let v2 = common.to_string()
        + "fn use_it() -> int {\n\
           \x20   let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]\n\
           \x20   return bad(xs)\n\
           }";
    let ws1 = ws_with_stdlib(&v1);
    let (full, _) = emit_full_of(&ws1, "emit_full v1");
    assert!(!full.lua.contains("function bad_"));

    let ws2 = ws_with_stdlib(&v2);
    let edited = [body_id(&ws2, "use_it")];
    let (_, mut incremental_cache) = emit_full_of(&ws1, "emit_full v1");
    let (_, mut overlay_cache) = emit_full_of(&ws1, "emit_full v1");
    let routes = [
        (
            "full",
            emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).map(|_| ()),
        ),
        (
            "incremental",
            emit_incremental(&ws2, &mut incremental_cache, &edited).map(|_| ()),
        ),
        (
            "overlay",
            emit_overlay_result(&ws2, &mut overlay_cache, &edited).map(|_| ()),
        ),
    ];
    for (route, result) in routes {
        let err = result.expect_err(route);
        assert!(err.message.contains("nobox") && err.message.contains("bad"));
    }
}

const MULTI_REGION_COROUTINE: &str = r#"
enum Ev { Tick(int) }
global LOG: int = 0
fn note(x: int) { LOG = x }
fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }
co fn worker(seed: int) -> Coroutine<Ev> {
    defer {
        note(apply((v) => v + seed, 1))
    }
    let base = apply((v) => v + 1, seed)
    yield Ev::Tick(base)
}
fn probe() -> int { return 1 }
fn main() {
    let c = worker(0)
    let _ = c.resume()
    let _ = probe()
}
"#;

#[test]
fn multi_region_coroutine_topology_gates_overlays() {
    let ws1 = ws_with_stdlib(MULTI_REGION_COROUTINE);
    let elsewhere = MULTI_REGION_COROUTINE.replace("return 1 }", "return 2 }");
    let (_, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let ws = ws_with_stdlib(&elsewhere);
    emit_overlay(&ws, &mut cache, &[body_id(&ws, "probe")]);

    let compatible = MULTI_REGION_COROUTINE.replace("v + seed, 1", "v + seed, 3");
    let inserted = compatible.replace(
        "        note(apply((v) => v + seed, 3))",
        "        note(apply((v) => v + seed, 3))\n        note(apply((v) => v - seed, 2))",
    );
    let (_, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let ws = ws_with_stdlib(&compatible);
    emit_overlay_result(&ws, &mut cache, &[body_id(&ws, "worker")])
        .expect("a body-only defer edit must overlay before testing lambda insertion");
    let ws = ws_with_stdlib(&inserted);
    let err = emit_overlay_result(&ws, &mut cache, &[body_id(&ws, "worker")])
        .map(|_| ())
        .expect_err("inserting a lambda into a region must refuse the overlay");
    assert!(err.message.contains("CodeRef compatibility") && err.message.contains("safe restart"));
}

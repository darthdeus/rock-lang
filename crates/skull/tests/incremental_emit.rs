//! Oracle for skull incremental codegen:
//! after re-emitting only the bodies a (simulated) edit re-inferred, the assembled
//! Lua must be BYTE-IDENTICAL to a fresh full emit of the edited workspace. The
//! interesting case is editing an INLINED callee -- its callers' chunks contain its
//! body, so they must re-emit too (the inline codegen-dep), even though a body edit
//! never re-infers the callers.

use crate::common::{body_id, emit_full_of, ws_of, ws_with_stdlib};
use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::{MirEmit, emit_full, emit_incremental, emit_overlay as emit_overlay_result};

fn emit_overlay(
    ws: &Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> MirEmit {
    emit_overlay_result(ws, cache, reinferred).expect("emit_overlay")
}

/// Drive one body edit through the cache and assert incremental == fresh.
fn check_edit(v1: &str, v2: &str, edited: &[&str]) -> String {
    check_edit_with(ws_of, v1, v2, edited)
}

#[test]
fn scalar_constant_edits_refresh_full_incremental_and_overlay_emission() {
    let v1 = "const BASE: float = 1.5\nconst ALIAS: float = BASE\n@nomangle fn choose() -> float { ALIAS }";
    let v2 = v1.replace("1.5", "-0.0");
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut incremental_cache) = emit_full(&ws1, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&ws1, mid).unwrap();
    assert!(crate::common::function_body(&full.lua, "choose").contains("1.5"));
    let ws2 = ws_of(&v2);
    let gid = ws2
        .module(mid)
        .globals
        .iter()
        .find(|(_, g)| g.name.as_str() == "BASE")
        .unwrap()
        .0;
    let edited = [(mid, BodyOwner::Global(gid))];
    let incremental = emit_incremental(&ws2, &mut incremental_cache, &edited).unwrap();
    let fresh = emit_full(&ws2, mid).unwrap().0;
    assert_eq!(incremental.lua, fresh.lua);
    let overlay = emit_overlay(&ws2, &mut overlay_cache, &edited);
    let body = crate::common::function_body(&overlay.lua, "choose");
    assert!(body.contains("-0.0") && !body.contains("ALIAS"), "{body}");
}

#[test]
fn derived_scalar_constant_edits_refresh_transitive_readers_at_runtime() {
    let template = "fn source() -> int { 32 }\nconst BASE: int = INIT\nconst SCALE: int = BASE * 128\nconst DERIVED: int = SCALE + 1\n@nomangle fn choose() -> int { DERIVED }";
    for (from, to, expected_before, expected_after, folded_after) in [
        ("8", "16", 1025, 2049, true),
        ("16", "source()", 2049, 4097, false),
        ("source()", "8", 4097, 1025, true),
    ] {
        let ws1 = ws_with_stdlib(&template.replace("INIT", from));
        let mid = skull::user_module_id(&ws1).unwrap();
        let (full, mut incremental_cache) = emit_full(&ws1, mid).unwrap();
        let (_, mut overlay_cache) = emit_full(&ws1, mid).unwrap();
        let ws2 = ws_with_stdlib(&template.replace("INIT", to));
        let (mid2, gid) = ws2
            .modules
            .iter()
            .find_map(|m| {
                m.globals
                    .iter()
                    .find(|(_, g)| g.name.as_str() == "BASE")
                    .map(|(id, _)| (m.id, id))
            })
            .unwrap();
        let edited = [(mid2, BodyOwner::Global(gid))];
        let incremental = emit_incremental(&ws2, &mut incremental_cache, &edited).unwrap();
        assert_eq!(incremental.lua, emit_full(&ws2, mid).unwrap().0.lua);
        let overlay = emit_overlay(&ws2, &mut overlay_cache, &edited);
        let after = crate::common::function_body(&overlay.lua, "choose");
        if folded_after {
            assert!(
                after.contains(&expected_after.to_string()) && !after.contains("DERIVED"),
                "{after}"
            );
        } else {
            assert!(after.contains("DERIVED"), "{after}");
        }
        let run = format!(
            "{}\nassert(choose() == {expected_before})\n{}\nassert(choose() == {expected_after})\nprint('ok')",
            full.lua, overlay.lua
        );
        assert_eq!(crate::common::run_luajit(&run), "ok");
    }
}

#[test]
fn numeric_selection_updates_in_full_incremental_and_overlay_emission() {
    let v1 =
        "@nomangle fn choose(a: float, b: float) -> float { return if a < b { a } else { b } }";
    let v2 = v1.replace("a < b", "a > b");
    let ws1 = ws_of(v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full");
    assert!(full.lua.contains("math.min("), "{}", full.lua);
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "choose")]);
    assert!(overlay.lua.contains("math.max("), "{}", overlay.lua);
    assert!(!overlay.lua.contains("math.min("), "{}", overlay.lua);
    check_edit(v1, &v2, &["choose"]);
}

/// [`check_edit`] over a chosen workspace builder. A fixture needing the stdlib
/// (any `+`, any collection method) passes `common::ws_with_stdlib`; the bare
/// `ws_of` keeps the cheap fixtures cheap.
fn check_edit_with(build: fn(&str) -> Workspace, v1: &str, v2: &str, edited: &[&str]) -> String {
    let ws1 = build(v1);
    let (_full1, mut cache) = emit_full_of(&ws1, "emit_full v1");

    let ws2 = build(v2);
    let mid2 = skull::user_module_id(&ws2).unwrap();
    let reinferred: Vec<_> = edited.iter().map(|n| body_id(&ws2, n)).collect();

    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let fresh = emit_full(&ws2, mid2).expect("emit_full v2").0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental assembly != fresh emit after editing {edited:?}"
    );
    incremental.lua
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

/// A transitive callee that starts or stops shrinking a vector must
/// invalidate the cached caller's bounds proof, in both incremental and
/// overlay emission.
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
        let hot = crate::common::function_body(lua, "hot");
        hot.contains("__vec_checked_get(") || hot.contains("__vec_checked_index(")
    };
    let ws1 = ws_with_stdlib(grow);
    let (full1, mut incremental_cache) = emit_full_of(&ws1, "emit_full v1");
    let (_, mut overlay_cache) = emit_full_of(&ws1, "overlay base v1");
    assert!(!checked(&full1.lua), "no writer in the chain: raw access");
    for (step, src, expect_check) in [("shrink", shrink.as_str(), true), ("grow", grow, false)] {
        let ws = ws_with_stdlib(src);
        let reinferred = [body_id(&ws, "leaf")];
        let incremental =
            emit_incremental(&ws, &mut incremental_cache, &reinferred).expect("incremental");
        let (fresh, _) = emit_full_of(&ws, "fresh");
        assert_eq!(incremental.lua, fresh.lua, "{step}: incremental != fresh");
        assert_eq!(
            checked(&incremental.lua),
            expect_check,
            "{step}: incremental"
        );
        let overlay = emit_overlay(&ws, &mut overlay_cache, &reinferred);
        assert_eq!(checked(&overlay.lua), expect_check, "{step}: overlay");
    }
}

#[test]
fn a_changed_caller_can_restage_an_unchanged_callback() {
    let v1 = r#"
fn apply<T>(value: T, callback: fn(T) -> T) -> T { return callback(value) }
fn unchanged(value: int) -> int { return value }
fn caller() -> int { return apply(1, unchanged) }
"#;
    let v2 = v1.replace("apply(1, unchanged)", "apply(2, unchanged)");
    let ws1 = ws_of(v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let callback = full
        .code_artifact
        .entries
        .values()
        .find(|entry| {
            entry.escaping
                && entry
                    .body_name
                    .is_some_and(|name| name.as_str().starts_with("unchanged"))
        })
        .expect("fixture must publish the unchanged callback");
    assert!(
        callback.body_name.is_some(),
        "fresh emission must realize the callback body"
    );

    let ws2 = ws_of(&v2);
    let overlay = emit_overlay_result(&ws2, &mut cache, &[body_id(&ws2, "caller")])
        .expect("an unchanged callback referenced by the changed caller must retain its body");
    let staged: Vec<_> = overlay
        .lua
        .lines()
        .filter(|line| line.contains("__rock_code_slot_stage(") && line.contains("unchanged"))
        .collect();
    assert_eq!(
        staged.len(),
        1,
        "the changed caller must stage its unchanged callback: {staged:?}"
    );
    assert!(
        staged[0].contains("unchanged_int"),
        "the callback stage must point at the body already loaded from the full artifact: {}",
        staged[0]
    );
}

/// The retained fast paths of a body edit (the root cache, the invalidated
/// candidate keys, the cached fingerprint rows) against their full-scan
/// equivalents. One edit sequence -- an inlined callee's body, a generic
/// instance removed, another added -- runs through a default cache and a
/// `set_full_scans` cache. Both must produce the same incremental Lua as a
/// fresh emit, and the same overlay and artifact fingerprint as each other.
#[test]
fn full_scans_agree_with_the_retained_fast_paths_over_an_edit_sequence() {
    let v0 = "@inline\nfn helper() -> int { 3 }\n\
              fn leaf<T>(x: T) -> T { x }\n\
              fn use_it() -> int { let a = leaf(1)\n helper() }\n\
              fn main() { let x = use_it() }";
    let steps = [
        (v0.replace("{ 3 }", "{ 5 }"), "helper"),
        (
            v0.replace("{ 3 }", "{ 5 }")
                .replace("let a = leaf(1)\n ", ""),
            "use_it",
        ),
        (
            v0.replace("{ 3 }", "{ 5 }")
                .replace("let a = leaf(1)", "let a = leaf(\"s\")"),
            "use_it",
        ),
        (v0.replace("leaf(1)", "leaf(\"s\")"), "helper"),
    ];
    let ws0 = ws_of(v0);
    let mid = skull::user_module_id(&ws0).unwrap();
    let cache = |full_scans: bool| {
        let (_, mut cache) = emit_full(&ws0, mid).expect("emit_full v0");
        cache.set_full_scans(full_scans);
        cache
    };
    let (mut incremental_fast, mut incremental_full) = (cache(false), cache(true));
    let (mut overlay_fast, mut overlay_full) = (cache(false), cache(true));
    for (step, (src, edited)) in steps.iter().enumerate() {
        let ws = ws_of(src);
        let reinferred = [body_id(&ws, edited)];
        let fast = emit_incremental(&ws, &mut incremental_fast, &reinferred).expect("fast");
        let full = emit_incremental(&ws, &mut incremental_full, &reinferred).expect("full scans");
        let fresh = emit_full_of(&ws, "fresh").0;
        assert_eq!(fast.lua, fresh.lua, "step {step}: fast path != fresh");
        assert_eq!(full.lua, fresh.lua, "step {step}: full scans != fresh");
        assert_eq!(
            fast.artifact_fingerprint, full.artifact_fingerprint,
            "step {step}: incremental fingerprints differ"
        );
        let (fast_stats, full_stats) = (
            incremental_fast.repair_stats(),
            incremental_full.repair_stats(),
        );
        assert!(
            fast_stats.root_functions_examined == 0 && full_stats.root_functions_examined > 0,
            "step {step}: only the full-scan cache enumerates roots: {fast_stats:?} {full_stats:?}"
        );
        let fast = emit_overlay(&ws, &mut overlay_fast, &reinferred);
        let full = emit_overlay(&ws, &mut overlay_full, &reinferred);
        assert_eq!(fast.lua, full.lua, "step {step}: overlays differ");
        assert_eq!(
            fast.artifact_fingerprint, full.artifact_fingerprint,
            "step {step}: overlay fingerprints differ"
        );
    }
}

#[test]
fn body_edit_to_a_function_with_lambdas_matches_a_full_emit() {
    let fixture = |step: i32| {
        format!(
            "fn apply(f: fn(int) -> int, x: int) -> int {{ return f(x) }}\n\
             fn lambdas(n: int) -> int {{\n\
                 let base = n * 2\n\
                 let a = (x: int) => x + base\n\
                 let b = (x: int) => {{\n\
                     if x > {step} {{ return x - base }}\n\
                     return apply(a, x)\n\
                 }}\n\
                 return apply(a, n) + apply(b, n + {step})\n\
             }}\n\
             fn other() -> int {{ return lambdas(3) }}"
        )
    };
    let (v1, v2) = (fixture(1), fixture(4));
    check_edit_with(ws_with_stdlib, &v1, &v2, &["lambdas"]);

    // The overlay redefines the hoisted lambda bodies exactly as a full emit
    // of the edited source prints them.
    let ws1 = ws_with_stdlib(&v1);
    let (_full1, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "lambdas")]).lua;
    let fresh = emit_full_of(&ws2, "emit_full v2").0.lua;
    let definition = |lua: &str, name: &str| -> Option<String> {
        let start = lua.find(&format!("function {name}("))?;
        let rest = &lua[start..];
        Some(rest[..rest.find("\nend\n")? + 4].to_string())
    };
    let lambdas: Vec<&str> = fresh
        .match_indices("__lambda")
        .filter_map(|(at, _)| {
            let start = fresh[..at].rfind("function ")? + "function ".len();
            let end = at + fresh[at..].find('(')?;
            (!fresh[start..end].contains(char::is_whitespace)).then(|| &fresh[start..end])
        })
        .collect();
    assert!(
        lambdas.len() >= 2,
        "the fixture hoists both lambdas:\n{fresh}"
    );
    for name in lambdas {
        let expected = definition(&fresh, name);
        assert!(expected.is_some(), "no full definition of `{name}`");
        assert_eq!(
            definition(&overlay, name),
            expected,
            "overlay and full emit disagree on `{name}`:\n{overlay}"
        );
    }
}

/// A sequence of edits to ONE cache, like a watch session: after each update
/// the assembly must still equal a fresh emit of that state. The first edit
/// is to `c`, inlined into `b` inlined into `a`, so a's chunk holds c's body.
#[test]
fn sequential_edits_stay_consistent() {
    let v0 = "@inline\nfn c() -> int { 1 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 9 }";
    let (_, mut cache) = emit_full_of(&ws_of(v0), "emit_full v0");
    let v1 = v0.replace("{ 1 }", "{ 2 }");
    for (src, edited) in [
        (v1.clone(), "c"),
        (v1.replace("{ 9 }", "{ 5 }"), "z"),
        (v1.replace("{ 9 }", "{ 7 }"), "z"),
    ] {
        let ws = ws_of(&src);
        let inc = emit_incremental(&ws, &mut cache, &[body_id(&ws, edited)]).unwrap();
        let (fresh, _) = emit_full_of(&ws, "fresh");
        assert_eq!(
            inc.lua, fresh.lua,
            "incremental drifted from fresh after editing {edited}"
        );
    }
}

/// The first overlay must ADD the Q specialization the edit made reachable
/// (main.lua has no definition, so omitting it is a nil global at reload).
/// A second edit to the generic must then reach Q too, which only holds if
/// the instance graph is maintained across edits rather than frozen at the
/// full build.
#[test]
fn a_later_edit_to_a_generic_reaches_an_instantiation_an_earlier_edit_added() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n";
    let generic = |marker: &str| {
        format!("fn tally_of<T>(xs: [T]) -> T {{ print(\"{marker}\") return xs[0] }}\n")
    };
    let v1 = types.to_string()
        + &generic("MARK1")
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return G1 }";
    // Edit 1: `use_it` gains the first call at `Q`.
    let v2 = types.to_string()
        + &generic("MARK1")
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";
    // Edit 2: the GENERIC's own body changes. Both instantiations must move.
    let v3 = types.to_string()
        + &generic("MARK2")
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let defines = |lua: &str, ty: &str| {
        lua.lines()
            .any(|l| l.starts_with("function tally_of_") && l.contains(ty))
    };
    let defines_q = |lua: &str| defines(lua, "Q");
    assert!(
        defines(&full.lua, "P") && !defines_q(&full.lua),
        "control: v1 defines P only"
    );

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let o1 = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert!(
        defines_q(&o1.lua),
        "control: edit 1 must add the Q instantiation:\n{}",
        o1.lua
    );

    let ws3 = crate::common::ws_with_stdlib(&v3);
    let o2 = emit_overlay(&ws3, &mut cache, &[body_id(&ws3, "tally_of")]);
    // The marker is what makes "re-emitted" observable; a name-only check would
    // pass on the stale definition.
    let bodies: Vec<&str> = o2
        .lua
        .lines()
        .filter(|l| l.starts_with("function tally_of_"))
        .collect();
    assert_eq!(
        bodies.len(),
        2,
        "both instantiations must be redefined, got {bodies:?}"
    );
    assert_eq!(
        o2.lua.matches("MARK2").count(),
        2,
        "each redefined body must carry the EDITED marker, or one instantiation \
         was served from a chunk the stale graph never invalidated:\n{}",
        o2.lua
    );
}

/// A pruned instance's chunk is forgotten, so an edit that brings the
/// instantiation back rebuilds it from the edited generic instead of serving
/// the chunk it had before it was pruned. A full reassembly never reaches the
/// stale chunk, so only the return exposes it.
#[test]
fn an_instantiation_that_returns_after_being_pruned_is_rebuilt_not_reused() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n";
    let generic = |marker: &str| {
        format!("fn tally_of<T>(xs: [T]) -> T {{ print(\"{marker}\") return xs[0] }}\n")
    };
    let keep_alive = "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n";
    let calls_q = "fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";
    let no_q = "fn use_it() -> float { return G1 }";

    let v1 = types.to_string() + &generic("MARK1") + keep_alive + calls_q;
    let v2 = types.to_string() + &generic("MARK1") + keep_alive + no_q; // strands Q
    let v3 = types.to_string() + &generic("MARK2") + keep_alive + no_q; // edits the generic
    let v4 = types.to_string() + &generic("MARK2") + keep_alive + calls_q; // Q returns

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let q_bodies = |lua: &str| -> Vec<String> {
        lua.lines()
            .filter(|l| l.starts_with("function tally_of_") && l.contains("Q"))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        q_bodies(&full.lua).len(),
        1,
        "control: v1 defines the Q instantiation"
    );

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let removal = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert_eq!(
        q_bodies(&removal.lua).len(),
        0,
        "the removal overlay must remove the stranded Q specialization:\n{}",
        removal.lua
    );
    let ws3 = crate::common::ws_with_stdlib(&v3);
    let _ = emit_overlay(&ws3, &mut cache, &[body_id(&ws3, "tally_of")]);

    let ws4 = crate::common::ws_with_stdlib(&v4);
    let back = emit_overlay(&ws4, &mut cache, &[body_id(&ws4, "use_it")]);
    assert_eq!(
        q_bodies(&back.lua).len(),
        1,
        "control: the returning instantiation must be defined again:\n{}",
        back.lua
    );
    let text = &back.lua;
    let start = text
        .find("function tally_of_")
        .map(|i| &text[i..])
        .unwrap_or(text);
    let q_start = text
        .find(q_bodies(text)[0].as_str())
        .map(|i| &text[i..])
        .unwrap_or(start);
    let q_body: String = q_start.lines().take_while(|l| *l != "end").collect();
    assert!(
        q_body.contains("MARK2"),
        "the returning instantiation must be REBUILT from the edited generic, not \
         served from the chunk it had before it was pruned:\n{q_body}"
    );
}

/// The overlay defines a non-generic library body the edit made reachable;
/// specializations reach the overlay by a different route. The subject must
/// be a stdlib fn: the compiled unit's own fns are roots, so they are never
/// graph-added.
#[test]
fn an_overlay_carries_a_graph_added_library_body() {
    let v1 = "fn use_it(c: int) -> bool { return c == 32 }\n\
              fn main() { print(\"{use_it(9)}\") }";
    let v2 = "fn use_it(c: int) -> bool { return is_ascii_ws(c) }\n\
              fn main() { print(\"{use_it(9)}\") }";

    let ws1 = crate::common::ws_with_stdlib(v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full v1");
    let defines = |lua: &str| {
        lua.lines()
            .any(|l| l.starts_with("function is_ascii_ws_int("))
    };
    assert!(
        !defines(&full.lua),
        "control: v1 must not define `is_ascii_ws`, or the overlay assertion below \
         is about a definition that already existed"
    );

    let ws2 = crate::common::ws_with_stdlib(v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert!(
        overlay.lua.contains("is_ascii_ws_int("),
        "control: the edited body must actually call it:\n{}",
        overlay.lua
    );
    assert!(
        defines(&overlay.lua),
        "the overlay must DEFINE the library body the edit made reachable, or the \
         reload calls a nil global:\n{}",
        overlay.lua
    );
}

/// Editing a generic `co fn` must re-emit its specialized coroutines, which
/// only the declaration edge in their deps connects to it. The column-stored
/// element is what makes the call specialize.
#[test]
fn a_body_edit_to_a_generic_coroutine_reemits_its_specializations() {
    let v1 = "struct Vec<T> { _data: any, len: int }\n\
              nobox value P { x: float, y: float }\n\
              co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {\n\
              \x20   let n = v.len\n\
              \x20   yield n\n\
              \x20   return 1\n\
              }\n\
              fn use_it() -> Coroutine<int, int, ()> {\n\
              \x20   let xs: [P] = [P { x: 1.0, y: 0.0 }]\n\
              \x20   return walk(xs)\n\
              }";
    let v2 = v1.replace("return 1", "return 2");
    // The population, asserted in the same test: without a specialized
    // coroutine in v1 there is nothing for the edit to invalidate and the
    // comparison would pass over an empty set.
    let ws = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full v1").0.lua;
    let spec_steps = lua
        .match_indices("__step(")
        .map(|(i, _)| {
            lua[..i]
                .rsplit("function ")
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|n| n.contains("_spec_"))
        .count();
    assert_eq!(
        spec_steps, 1,
        "the fixture must produce a SPECIALIZED coroutine step:\n{lua}"
    );
    check_edit(v1, &v2, &["walk"]);
}

/// A standalone `Iterator`-bounded body survives an incremental rebuild. Its
/// placeholders are resolved from the demand's tree, not from the body, so the
/// tree must live in the cache beside the chunk key. Inlining is off, so the
/// bounded body has no splice to fall back on.
#[test]
fn a_standalone_bounded_body_survives_an_incremental_rebuild() {
    let _inline = crate::common::InlineOffGuard::on();
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
        crate::common::definitions_matching(lua, "_spec_")
            .into_iter()
            .filter(|name| name.starts_with("total_"))
            .count()
    };
    let (full1, _) = emit_full_of(&ws_with_stdlib(v1), "emit_full v1");
    assert_eq!(
        standalone(&full1.lua),
        1,
        "v1 must emit ONE standalone bounded body"
    );
    let incremental = check_edit_with(ws_with_stdlib, v1, &v2, &["total"]);
    assert_eq!(
        standalone(&incremental),
        1,
        "the rebuilt bounded body is gone -- its demand tree did not survive the cache"
    );
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
    check_edit_with(ws_with_stdlib, v1, &v2, &["stream"]);
    let ws1 = ws_with_stdlib(v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full");
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "stream")]);
    let (fresh, _) = emit_full_of(&ws2, "emit_full");
    let step = |lua: &str| {
        let start = lua.find("function stream_0__step(").expect("step function");
        let len = lua[start..].find("\nend\n").expect("step end");
        lua[start..start + len].to_string()
    };
    assert_eq!(
        step(&overlay.lua),
        step(&fresh.lua),
        "overlay frame stores must use the same guards as a full build"
    );
    let run = format!(
        "{}\nprint(sample(1))\n{}\nprint(sample(3))\n",
        full.lua, overlay.lua
    );
    assert_eq!(crate::common::run_luajit(&run), "true\ntrue");
}

const SHARED_ABI_SLOTS: &str = r#"
@lifecycle(phase="update", owner="process")
deco init tick {}
@tick fn first() {}
@tick fn second() {}
@tick fn third() {}
fn main() {}
"#;

#[test]
fn code_slot_staging_names_each_distinct_call_abi_once() {
    let ws = ws_of(SHARED_ABI_SLOTS);
    let (full, _cache) = emit_full_of(&ws, "emit_full");
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
    assert!(
        staged > abis.len(),
        "fixture must share a call ABI across slots: {staged} slots, {} ABIs",
        abis.len()
    );
    for abi in abis {
        let literal = skull_codegen_lua::lua_string(abi);
        assert_eq!(
            full.lua.matches(literal.as_str()).count(),
            1,
            "ABI {literal} must be named once"
        );
    }
}

#[test]
fn a_shared_call_abi_stays_compatible_across_an_overlay() {
    let v2 = SHARED_ABI_SLOTS.replace("@tick fn first() {}", "@tick fn first() { let x = 1 }");
    let ws1 = ws_of(SHARED_ABI_SLOTS);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full");
    let escaping: Vec<_> = full
        .code_artifact
        .entries
        .values()
        .filter(|entry| entry.escaping)
        .collect();
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(
        overlay.lua.contains("__rock_code_slot_stage("),
        "the overlay must restage `first`"
    );
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
print(before)
"#,
        full = full.lua,
        overlay = overlay.lua
    );
    let out = crate::common::run_luajit(&run);
    assert_eq!(
        out.lines().count(),
        escaping.len(),
        "every escaping CodeRef has a slot:\n{out}"
    );
    for entry in escaping {
        let abi = format!("={}", entry.compatibility.call_abi);
        assert!(
            out.lines().any(|row| row.ends_with(&abi)),
            "no slot carries ABI {abi}:\n{out}"
        );
    }
}

#[test]
fn fingerprint_code_rows_include_new_body_specializations() {
    let original =
        "fn identity<T>(value: T) -> T { return value }\nfn main() { let _ = identity(1) }";
    let changed = original.replace(
        "let _ = identity(1)",
        "let _ = identity(1)\nlet _ = identity(true)",
    );
    let ws1 = ws_of(original);
    let (baseline, mut cache) = emit_full_of(&ws1, "emit_full");
    let ws2 = ws_of(&changed);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "main")]);
    assert!(overlay.code_artifact.entries.len() > baseline.code_artifact.entries.len());
    assert_ne!(overlay.artifact_fingerprint, baseline.artifact_fingerprint);
}

#[test]
fn fingerprint_code_rows_survive_rollback() {
    let original = "fn helper() -> int { return 1 }\nfn main() { let _ = helper() }";
    let ws1 = ws_of(original);
    let (_, mut cache) = emit_full_of(&ws1, "emit_full");
    let ws2 = ws_of(&original.replace("return 1", "return 2"));
    let edited = [body_id(&ws2, "helper")];
    cache.begin_body_edit_transaction(&edited, &[]);
    let _ = emit_overlay(&ws2, &mut cache, &edited);
    cache.rollback_body_edit(&ws1).expect("rollback");
    for value in [3, 4] {
        let changed = ws_of(&original.replace("return 1", &format!("return {value}")));
        let overlay = emit_overlay(&changed, &mut cache, &[body_id(&changed, "helper")]);
        let fresh = emit_full_of(&changed, "emit_full").0;
        assert_eq!(overlay.artifact_fingerprint, fresh.artifact_fingerprint);
    }
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
    assert!(
        err.message.contains("CodeRef compatibility") && err.message.contains("safe restart"),
        "{}",
        err.message
    );
}

#[test]
fn flat_ref_abi_uses_logical_layout() {
    let ws = ws_with_stdlib(
        r#"
value Cell { v: int }
fn bump(c: ref Cell) -> int { c.v += 1
return c.v }
fn sample() -> int {
    let xs: [Cell] = []
    xs.push(Cell { v: 10 })
    return bump(xs[0])
}
"#,
    );
    let (full, _) = emit_full_of(&ws, "emit_full");
    let flat_abis: Vec<_> = full
        .code_artifact
        .entries
        .values()
        .map(|entry| entry.compatibility.call_abi.as_str())
        .filter(|abi| abi.contains("|flat "))
        .collect();
    assert!(
        !flat_abis.is_empty(),
        "the production artifact must contain a flat variant"
    );
    assert!(
        flat_abis.iter().all(|abi| !abi.contains("RepId")),
        "call ABI must not contain build-scoped arena handles: {flat_abis:?}"
    );
}

/// A body edit that starts stringifying a payload-free enum needs that enum's
/// names table, which the full build did not emit because nothing read it.
#[test]
fn newly_stringified_enum_gets_its_names_table_in_incremental_and_overlay() {
    let v1 = r#"
enum Mode { Idle, Busy }
@nomangle fn describe(m: Mode) -> string { return "mode" }
fn main() { print(describe(Mode::Busy)) }
"#;
    let v2 = v1.replace("return \"mode\"", "return \"{m}\"");
    check_edit_with(ws_with_stdlib, v1, &v2, &["describe"]);
    let ws1 = ws_with_stdlib(v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full");
    assert!(!full.lua.contains("Mode::Busy"), "{}", full.lua);
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "describe")]);
    let run = format!("{}\n{}\nprint(describe(1))\n", full.lua, overlay.lua);
    assert_eq!(crate::common::run_luajit(&run), "Mode::Busy");
}

/// Body edits whose only obligation is that the incremental assembly equals a
/// fresh full emit of the edited source.
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
    // `mk` returns a `value` struct, inlined into `use_v` as a scalarized
    // CallMulti: the inline dependency must cover CallMulti callees too.
    let ctor = "value V2 { x: int, y: int }\n\
                @inline\nfn mk() -> V2 { V2 { x: 1, y: 2 } }\n\
                fn use_v() -> int { let v = mk()\n v.x }";
    // Removing the last call retracts the generic instance and its chunk.
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
        check_edit_with(build, v1, &v2, &[edited]);
    }
}

/// An inlinable helper's body edit keeps, keeps residual, or newly enables the
/// splice into its caller; the caller's chunk must follow each decision.
#[test]
fn inline_decisions_follow_helper_edits() {
    let small = |value: i32| {
        format!(
            "@inline @nomangle fn helper() -> int {{ {value} }}\n\
             @nomangle fn caller() -> float {{ helper() }}\n\
             @nomangle fn other() -> int {{ 7 }}"
        )
    };
    // The caller starts over the 400-cell splice budget.
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
    for (label, v1, v2, residual_before, residual_after) in [
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
        let residual = |lua: &str| crate::common::function_body(lua, "caller").contains("helper(");
        let (full1, _) = emit_full_of(&ws_of(&v1), "emit_full v1");
        assert_eq!(residual(&full1.lua), residual_before, "{label}: v1 caller");
        let incremental = check_edit(&v1, &v2, &["helper"]);
        assert_eq!(residual(&incremental), residual_after, "{label}: v2 caller");
    }
}

/// Decorated bodies publish CodeRef slots. An overlay restages exactly the
/// slots whose bodies changed, after their new definitions, under logical
/// identity, and always ends in the transactional publication.
#[test]
fn overlays_restage_only_changed_code_refs() {
    let v1 = r#"
@lifecycle(phase="update", owner="process")
deco init tick {}
@tick fn first() {}
@tick fn second() {}
fn helper() -> int { return 1 }
fn main() { let _ = helper() }
"#;
    let ws1 = ws_of(v1);
    let (full, _) = emit_full_of(&ws1, "emit_full v1");
    let escaping = full
        .code_artifact
        .entries
        .values()
        .filter(|entry| entry.escaping)
        .count();
    assert!(escaping >= 2, "fixture must expose CodeRefs: {escaping}");

    for (v2, edited, staged_count) in [
        (v1.replace("return 1", "return 2"), "helper", 0),
        (
            v1.replace("@tick fn first() {}", "@tick fn first() { let x = 1 }"),
            "first",
            1,
        ),
    ] {
        let (_, mut cache) = emit_full_of(&ws1, "emit_full v1");
        let ws2 = ws_of(&v2);
        let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, edited)]).lua;
        let staged: Vec<_> = overlay
            .lines()
            .filter(|line| line.contains("__rock_code_slot_stage("))
            .collect();
        assert_eq!(staged.len(), staged_count, "{edited}: {staged:?}");
        assert!(
            overlay.contains("__rock_code_slot_publish()"),
            "{edited}: the overlay must retain the publication boundary"
        );
        if let Some(stage) = staged.first() {
            assert!(stage.contains("named:<t>::first<>"), "{stage}");
            let stage_offset = overlay.find("__rock_code_slot_stage(").unwrap();
            let body_offset = overlay.find("function first").expect("changed body");
            assert!(
                body_offset < stage_offset,
                "publication must observe the new body: {body_offset} !< {stage_offset}"
            );
        }
    }
}

/// A specialized body is not reached through its declaration's own chunk, so
/// only the declaration edge in its deps re-emits it after the generic
/// changes. The global initializer never runs an inliner, which forces a
/// standalone specialized copy. A reload dofiles the overlay after main.lua,
/// so the overlay must redefine that copy too.
#[test]
fn body_edit_to_a_generic_reemits_its_specializations() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let v2 = v1.replace("xs[0]", "xs[1]");
    let spec = "function first_arr_T_spec_a_n2_V2_0_z(";
    let ws1 = ws_of(v1);
    let (full, mut cache) = emit_full_of(&ws1, "emit_full v1");
    // One liveness walk decides the specialized body, so it is emitted once.
    assert_eq!(full.lua.matches(spec).count(), 1, "{}", full.lua);

    check_edit(v1, &v2, &["first"]);
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(
        overlay.lua.contains(spec),
        "the overlay must redefine the specialization the edit moved:\n{}",
        overlay.lua
    );
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

/// Taking `foo` by value flips its scalar ABI width; the overlay must redefine
/// `foo` and its caller rather than keep definitions built under the old ABI.
#[test]
fn an_overlay_reemits_a_definition_after_a_scalar_abi_width_flip() {
    let ws1 = ws_with_stdlib(ABI_FLIP_V1);
    let (full1, mut cache) = emit_full_of(&ws1, "emit_full v1");
    assert!(full1.lua.contains("function foo_"), "v1 must emit foo");

    let ws2 = ws_with_stdlib(ABI_FLIP_V2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_ref")]);
    assert!(
        overlay.lua.contains("function foo_") && overlay.lua.contains("function use_ref"),
        "the ABI-width flip must re-emit foo and its caller in the overlay:\n{}",
        overlay.lua
    );
}

/// A graph-added specialization whose body fails construction fails closed on
/// every route: full, incremental and overlay.
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
    assert!(!full.lua.contains("function bad_"), "v1 must not emit it");

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
        assert!(
            err.message.contains("nobox") && err.message.contains("bad"),
            "{route}: the failure must identify the rejected specialization: {}",
            err.message
        );
    }
}

/// A caller edit adds or removes a specialization demand while a sibling
/// instance of the same generic stays reached, so invalidation must be per
/// instance. The fresh counts make the incremental equality non-vacuous.
#[test]
fn caller_edits_add_and_prune_specializations_per_instance() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n\
                 fn tally_of<T>(xs: [T]) -> T { return xs[0] }\n";
    let one = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   return tally_of(ps).x\n\
           }";
    let both = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   let qs: [Q] = [Q { a: 1.0, b: 2.0, c: 3.0 }]\n\
           \x20   return tally_of(ps).x + tally_of(qs).a\n\
           }";
    let specs = |src: &str| {
        let (full, _) = emit_full_of(&ws_with_stdlib(src), "emit_full");
        full.lua
            .lines()
            .filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_"))
            .count()
    };
    assert_eq!((specs(&one), specs(&both)), (1, 2));
    check_edit_with(ws_with_stdlib, &one, &both, &["use_it"]);
    check_edit_with(ws_with_stdlib, &both, &one, &["use_it"]);
}

/// A changed call graph that newly reaches an inline(lua) publisher repairs
/// the artifact's scalar context to match a fresh emit.
#[test]
fn changed_call_graph_reaching_a_foreign_publisher_matches_full_emission() {
    let v1 = "fn helper() -> int { 3 }\ninline(lua) fn extra() -> int = \"return 7\"\nfn main() { let x = helper() }";
    let v2 = v1.replace("{ 3 }", "{ extra() }");
    let ws1 = ws_with_stdlib(v1);
    let (_, mut cache) = emit_full_of(&ws1, "initial emit");
    let ws2 = ws_with_stdlib(&v2);
    let incremental =
        emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "helper")]).expect("incremental emit");
    assert!(
        cache
            .lua_view()
            .publisher_lanes
            .keys()
            .any(|i| ws2.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "extra"),
        "the changed call graph must reach a foreign publisher"
    );
    let fresh = emit_full_of(&ws2, "fresh emit").0;
    assert_eq!(incremental.lua, fresh.lua);
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

/// A `co fn` with lambdas in its step region and a detached defer region has
/// one lambda topology per region under a single owner. Body-only edits, to
/// another function or inside the defer region, must overlay; inserting a
/// lambda into the region must refuse with the compatibility restart.
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
    assert!(
        err.message.contains("CodeRef compatibility") && err.message.contains("safe restart"),
        "the refusal must stay the compatibility restart: {}",
        err.message
    );
}

#[test]
fn repeated_body_overlays_keep_the_fingerprint_of_a_full_emit() {
    let mut source =
        String::from("fn helper() -> int { return 1 }\nfn main() { let _ = helper() }\n");
    for index in 0..40 {
        source.push_str(&format!(
            "fn unrelated{index}() -> int {{ return {index} }}\n"
        ));
    }
    let original = ws_of(&source);
    let (_, mut cache) = emit_full_of(&original, "emit_full v1");
    for value in [2, 3, 4] {
        let changed = ws_of(&source.replacen("return 1 }", &format!("return {value} }}"), 1));
        let overlay = emit_overlay(&changed, &mut cache, &[body_id(&changed, "helper")]);
        assert!(overlay.code_artifact.entries.len() > 40);
        let (fresh, _) = emit_full_of(&changed, "emit_full v2");
        assert_eq!(overlay.artifact_fingerprint, fresh.artifact_fingerprint);
    }
}

/// An overlay of a `ref` callee's body updates the variant its flat-element
/// caller runs, whether that caller is plain or specialized.
#[test]
fn flat_ref_callee_body_overlay_updates_its_callers() {
    crate::common::require_luajit();
    let prelude = r#"
value Cell { v: int }
global xs: [Cell] = []
fn apply(c: ref Cell, delta: int) -> int { c.v += delta
return c.v }
@nomangle fn seed() { xs.push(Cell { v: 10 }) }
@nomangle fn snapshot() -> int { return xs[0].v }
"#;
    let plain = "@nomangle fn tick(delta: int) -> bool {
    let before = xs[0].v
    let seen = apply(xs[0], delta)
    return seen != before && xs[0].v == seen
}";
    let specialized = "fn step<T>(_tag: T, delta: int) -> bool {
    let before = xs[0].v
    let seen = apply(xs[0], delta)
    return seen != before && xs[0].v == seen
}
@nomangle fn tick(delta: int) -> bool { return step(1, delta) }";
    for caller in [plain, specialized] {
        let v1 = format!("{prelude}{caller}\n");
        let v2 = v1.replace("c.v += delta", "c.v -= delta");
        let ws1 = ws_with_stdlib(&v1);
        let (full, mut cache) = emit_full_of(&ws1, "emit_full v1");
        let ws2 = ws_with_stdlib(&v2);
        let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "apply")]);
        let run = format!(
            "{}\nseed()\nassert(tick(2))\nlocal before = snapshot()\n{}\nassert(snapshot() == before)\nassert(tick(3))\nassert(snapshot() < before)\nprint('ok')\n",
            full.lua, overlay.lua,
        );
        assert_eq!(crate::common::run_luajit(&run), "ok", "{caller}");
    }
}

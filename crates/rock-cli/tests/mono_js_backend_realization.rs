//! **The JS artifact and the JS emitter read ONE backend realization oracle**
//! (`docs/monomorphization-plan.md` S 9).
//!
//! `skull_codegen_js::emit::js_supply` answers "does this backend supply this
//! declaration itself, and how" for everything the backend supplies: the
//! builtin-`Map` receiver interceptions, the `print`/`put`/logging globals, the
//! hand-written host templates, and the `@intrinsic` table. `render_call`
//! renders from it, `discover_callees` skips on it, and the artifact view reads
//! it through `JsBackendRealization`.
//!
//! `skull::artifact` cannot depend on `skull-codegen-js` -- the backend is
//! driven from here -- so the view takes the oracle through one seam. That seam
//! is only worth anything if what goes through it is the oracle ITSELF and the
//! artifact's precedence is the emitter's: the callee walk skips a supplied
//! callee BEFORE it reaches the `inline(js)` co-emit and plain-Rock branches,
//! so a supplied declaration is never emitted whatever body it has.
//!
//! `map_new` is the case that makes it load-bearing: pure Rock
//! (`transmute(table_new())`) rendered as `new Map()`. A view that classified it
//! by its BODY would call it a Rock body and walk into callees this artifact
//! never emits.

use bone_semantic::inference::Workspace;
use skull::artifact::{self, ArtifactMappings, ArtifactRequest, Realization};
use skull::instance_graph;
use skull_codegen_js::JsBackendRealization;
use ustr::Ustr;

fn check(src: &str) -> Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Every shape the oracle has to separate, in one program:
///   - `map_new`         : a pure ROCK body the backend renders `new Map()`
///   - `Map.get`         : a builtin-receiver interception
///   - `error`           : a hand-written host template with no marker
///   - `rock_body`       : an ordinary Rock body, co-emitted
///   - `js_body`         : an `inline(js)` body, co-emitted verbatim
///   - `lua_only`        : nothing supplies it on JS -- a rejection
const MIXED: &str = r#"
fn rock_body(x: int) -> int { return x + 1 }
inline(js) fn js_body(x: int) -> int = "return x + 2;"
inline(lua) fn lua_only(x: int) -> int = "return x + 3"

fn main() {
    let m: Map<string, int> = map_new()
    m["a"] = 1
    let hit = m.get("a")
    let a = rock_body(1)
    let b = js_body(2)
    let c = lua_only(3)
    if a < 0 { error("no") }
    print("{hit ?? -1} {a} {b} {c}")
}
"#;

fn view(ws: &Workspace, wired: bool) -> artifact::ArtifactView {
    let roots = skull::mir_emit::emission_roots(ws, None);
    let g = instance_graph::build(ws, roots.clone(), false);
    let supply = JsBackendRealization;
    let req = ArtifactRequest::new(skull::target_projection::JAVASCRIPT_WEB, &roots);
    let req = if wired {
        req.with_mappings(
            ArtifactMappings::for_backend(bone_semantic::hir::FnBackend::Js)
                .with_supply(&supply),
        )
    } else {
        req
    };
    artifact::realize(ws, &g, &req)
}

fn kinds(ws: &Workspace, v: &artifact::ArtifactView, name: &str) -> Vec<Realization> {
    v.realization
        .iter()
        .filter(|(i, _)| ws.module(i.func.module).functions[i.func.func].name.as_str() == name)
        .map(|(_, r)| r.kind)
        .collect()
}

fn retained_rock(ws: &Workspace, v: &artifact::ArtifactView, name: &str) -> bool {
    v.instances
        .iter()
        .any(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == name)
}

#[test]
fn the_js_artifact_realizes_what_the_js_oracle_supplies() {
    let ws = check(MIXED);
    let wired = view(&ws, true);

    // `map_new` is a ROCK body the JS backend renders as `new Map()`. Supplied,
    // so it is a BOUNDARY and nothing walks into the body it replaces.
    let map_new = kinds(&ws, &wired, "map_new");
    assert!(!map_new.is_empty(), "the fixture retained no `map_new` instance");
    assert!(
        map_new.iter().all(|k| *k == Realization::HostBinding),
        "`map_new` is not supplied by the JS artifact: {map_new:?}"
    );
    assert!(
        wired.boundaries.keys().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "map_new"
        }),
        "`map_new` is supplied but not retained as a boundary"
    );
    assert!(!retained_rock(&ws, &wired, "map_new"), "the walk entered `map_new`'s Rock body");

    // The builtin `Map` receiver's `get`, rendered `.get(k)` at the call site.
    assert!(
        kinds(&ws, &wired, "get").contains(&Realization::HostBinding),
        "the builtin Map `get` is not supplied by the JS artifact"
    );

    // A host TEMPLATE with no `@intrinsic` marker at all.
    assert!(
        kinds(&ws, &wired, "error").contains(&Realization::HostBinding),
        "`error` is not supplied by the JS artifact"
    );

    // An ORDINARY Rock body is co-emitted, not supplied -- the oracle is not
    // simply answering yes.
    assert_eq!(kinds(&ws, &wired, "rock_body"), vec![Realization::RockBody]);
    assert!(retained_rock(&ws, &wired, "rock_body"));

    // An `inline(js)` body is a foreign body the artifact emits, not a supplied
    // one: the backend wrote the SOURCE body, which is a different answer.
    assert_eq!(
        kinds(&ws, &wired, "js_body"),
        vec![Realization::ForeignBody(instance_graph::BoundaryClass::InlineBody)]
    );

    // ...and a Lua-only body nothing supplies on JS is a REJECTION.
    assert!(
        wired.excluded.iter().any(|e| {
            ws.module(e.instance.func.module).functions[e.instance.func.func].name.as_str()
                == "lua_only"
        }) || wired.severed.iter().any(|s| {
            ws.module(s.callee.func.module).functions[s.callee.func.func].name.as_str()
                == "lua_only"
        }),
        "an unmapped Lua-only body was realized on the JS artifact"
    );

    // CONTROL: without the seam wired, the very same declarations are
    // classified by their bodies -- which is the divergence this measures, not
    // a property of the fixture.
    let plain = view(&ws, false);
    assert!(
        kinds(&ws, &plain, "map_new").contains(&Realization::RockBody),
        "`map_new` is not a Rock body even unsupplied, so the oracle proves nothing"
    );
    assert!(retained_rock(&ws, &plain, "map_new"), "the unwired view did not traverse `map_new`");
}

/// **The artifact's answer IS the emitter's**, over every declaration in the
/// workspace rather than a list this test keeps. A disagreement anywhere is a
/// call the artifact reports one way and the emitter renders another.
#[test]
fn the_artifact_and_the_emitter_agree_on_every_declaration() {
    use skull_mir::intrinsics::BackendRealization;
    let ws = check(MIXED);
    let supply = JsBackendRealization;
    let mut supplied = 0usize;
    let mut examined = 0usize;
    for (_, mid) in ws.module_ids.iter() {
        for (_, f) in ws.module(*mid).functions.iter() {
            examined += 1;
            let seam = supply.supplies(&ws, f);
            let emitter = skull_codegen_js::emit::js_supply(&ws, f).is_some();
            assert_eq!(
                seam, emitter,
                "the seam and the emitter disagree on `{}` (seam={seam}, emitter={emitter})",
                f.name
            );
            if seam {
                supplied += 1;
            }
        }
    }
    // Both halves of the population, so neither a vacuous zero nor an
    // everything-answers oracle passes.
    assert!(examined > 500, "only {examined} declarations examined");
    assert!(supplied > 20, "the backend supplies only {supplied} of {examined} declarations");
    assert!(supplied < examined, "the oracle answered for every declaration");
}

/// **Precedence is part of the oracle.** `map_new`'s Rock body must never win
/// ahead of the `new Map()` rendering: the emitter skips it in the callee walk
/// before any body branch, so an artifact that ordered the questions the other
/// way would traverse a body emission never emits. This pins the emitter side
/// of that ordering, which the artifact assertion above pins on its own side.
#[test]
fn the_oracle_supplies_map_new_ahead_of_its_rock_body() {
    let ws = check(MIXED);
    let map_new = ws
        .module_ids
        .iter()
        .flat_map(|(_, mid)| ws.module(*mid).functions.iter().map(|(_, f)| f))
        .find(|f| f.name.as_str() == "map_new" && f.param_decls.is_empty())
        .expect("`map_new` declaration");
    // It genuinely has a plain Rock body -- no foreign body, no `@intrinsic`.
    assert!(map_new.inline_body.is_none(), "`map_new` is not a plain Rock body any more");
    assert!(!map_new.is_extern);
    assert_eq!(
        skull_codegen_js::emit::js_supply(&ws, map_new),
        Some(skull_codegen_js::emit::JsSupply::Template("new Map()")),
        "the oracle stopped rendering `map_new` as a real JS Map"
    );
}


/// **One oracle, one rung -- so the LABEL comes from the declaration, not from
/// which table answered.**
///
/// Eleven stdlib declarations carry `@intrinsic` AND are mapped by the JS host
/// template table rather than by `JsIntrinsics` (`table_get`, `float`, `find`,
/// `substring`, ...). Asking "is it host-bound?" before "is it a mapped
/// intrinsic?" labelled those `host-binding`; they are `@intrinsic`
/// declarations this backend maps, so they are `native-mapping`. Corpus-wide
/// the change is exactly 211 realizations relabelled and nothing else -- both
/// labels carry the same `BoundaryClass::Intrinsic` contract.
///
/// That the mapping lives in the UNCHECKED table is `docs/unfuck-backlog.md`
/// item 74, not a difference in what the artifact owes.
#[test]
fn an_intrinsic_the_host_table_maps_is_a_native_mapping() {
    let ws = check(MIXED);
    let wired = view(&ws, true);
    // `table_get` is `@intrinsic` and its JS mapping is a host template.
    let decl = ws
        .module_ids
        .iter()
        .flat_map(|(_, mid)| ws.module(*mid).functions.iter().map(|(_, f)| f))
        .find(|f| f.name.as_str() == "table_get" && f.param_decls.len() == 2)
        .expect("`table_get` declaration");
    assert!(
        decl.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic"),
        "`table_get` is no longer `@intrinsic`, so this measures nothing"
    );
    assert!(skull_codegen_js::emit::js_supply(&ws, decl).is_some());
    let mapped = kinds(&ws, &wired, "table_get");
    if !mapped.is_empty() {
        assert!(
            mapped.iter().all(|k| *k == Realization::NativeMapping),
            "an `@intrinsic` the backend maps was labelled something else: {mapped:?}"
        );
    }
    // ...and `error`, which carries NO marker, stays `host-binding`. The two
    // labels are separated by the DECLARATION, which is the only thing that
    // distinguishes them once there is one oracle.
    assert!(kinds(&ws, &wired, "error").contains(&Realization::HostBinding));
}

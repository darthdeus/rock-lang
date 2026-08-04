//! The JS artifact's HOST BINDINGS are the JS emitter's own, over the real
//! declarations (`docs/monomorphization-plan.md` S 9).
//!
//! `skull::artifact` cannot depend on `skull-codegen-js` -- the backend is
//! driven from here -- so the view takes the predicate through a seam. That
//! seam is only worth anything if the value handed through it is the emitter's
//! ACTUAL mapping, and if the artifact's precedence matches the emitter's:
//! `skull_codegen_js`'s callee walk skips a host-bound callee BEFORE it reaches
//! the co-emit branches, so a bound declaration is never emitted whatever body
//! it has.
//!
//! `map_new` is the case that makes it load-bearing: pure Rock
//! (`transmute(table_new())`) rendered as `new Map()`. A view that classified
//! it by its BODY would call it a Rock body and walk into callees this artifact
//! never emits. The builtin `Map` receiver's `get`/2 is the same shape one rung
//! down (rendered `.get(k)` inline).

use bone_semantic::inference::Workspace;
use skull::artifact::{self, ArtifactMappings, ArtifactRequest, Realization};
use skull::instance_graph;
use ustr::Ustr;

/// The real predicate, exactly as `rock mono-manifest` wires it.
struct JsHostBindings;

impl artifact::HostBindings for JsHostBindings {
    fn supplies(&self, ws: &Workspace, f: &bone_semantic::hir::Function) -> bool {
        skull_codegen_js::emit::host_binding(ws, f)
    }
    fn describe(&self) -> &'static str {
        "js-host-bindings"
    }
}

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

const MAP_USER: &str = r#"
fn main() {
    let m: Map<string, int> = map_new()
    m["a"] = 1
    let hit = m.get("a")
    print("{hit ?? -1}")
}
"#;

#[test]
fn the_js_artifact_binds_the_declarations_its_emitter_renders_itself() {
    let ws = check(MAP_USER);
    let roots = instance_graph::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots.clone(), false);
    let bindings = JsHostBindings;
    let view = |with: bool| {
        let req = ArtifactRequest::new(skull::target_projection::JAVASCRIPT_WEB, &roots);
        let req = if with {
            req.with_mappings(
                ArtifactMappings::for_backend(bone_semantic::hir::FnBackend::Js)
                    .with_host_bindings(&bindings),
            )
        } else {
            req
        };
        artifact::realize(&ws, &g, &req)
    };
    let kinds = |v: &artifact::ArtifactView, name: &str| -> Vec<Realization> {
        v.realization
            .iter()
            .filter(|(i, _)| {
                ws.module(i.func.module).functions[i.func.func].name.as_str() == name
            })
            .map(|(_, r)| r.kind)
            .collect()
    };

    let bound = view(true);
    // `map_new` is a ROCK body the JS backend renders as `new Map()`.
    let map_new = kinds(&bound, "map_new");
    assert!(!map_new.is_empty(), "the fixture retained no `map_new` instance");
    assert!(
        map_new.iter().all(|k| *k == Realization::HostBinding),
        "`map_new` is not bound by the JS artifact: {map_new:?}"
    );
    // ...and it is a BOUNDARY, so nothing walks into the body it replaces.
    assert!(
        bound.boundaries.keys().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "map_new"
        }),
        "`map_new` is bound but not retained as a boundary"
    );
    assert!(
        !bound.instances.iter().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "map_new"
        }),
        "the walk entered `map_new`'s Rock body"
    );

    // The builtin `Map` receiver's `get`, rendered `.get(k)` at the call site.
    let get_bound: Vec<Realization> = kinds(&bound, "get")
        .into_iter()
        .filter(|k| *k == Realization::HostBinding)
        .collect();
    assert!(!get_bound.is_empty(), "the builtin Map `get` is not bound by the JS artifact");

    // CONTROL: without the seam wired, the very same declarations are
    // classified by their bodies -- which is the divergence this measures, not
    // a property of the fixture.
    let plain = view(false);
    assert!(
        kinds(&plain, "map_new").iter().any(|k| *k == Realization::RockBody),
        "`map_new` is not a Rock body even unbound, so the binding proves nothing"
    );
    assert!(
        plain.instances.iter().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "map_new"
        }),
        "the unbound view did not traverse `map_new` either"
    );
}

/// The predicate is the EMITTER's, not a list this test keeps: every name it
/// answers for is one `skull_codegen_js` renders itself.
#[test]
fn the_seam_carries_the_emitters_own_predicate() {
    let ws = check(MAP_USER);
    let bindings = JsHostBindings;
    let mut bound = 0usize;
    for (_, mid) in ws.module_ids.iter() {
        for (_, f) in ws.module(*mid).functions.iter() {
            if artifact::HostBindings::supplies(&bindings, &ws, f) {
                assert!(
                    skull_codegen_js::emit::host_binding(&ws, f),
                    "the seam answered for `{}` where the emitter does not",
                    f.name
                );
                bound += 1;
            }
        }
    }
    // The stdlib's host-bodied surface is large; a handful would mean the
    // predicate is answering for almost nothing and the test above is thin.
    assert!(bound > 20, "the emitter binds only {bound} declarations, unexpectedly few");
}

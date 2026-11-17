//! Workspace builders shared by this harness's tests.
use std::sync::{Arc, LazyLock};

use skull_mir::concrete::{ConcreteBodyPlan, ConcreteOwner, ConcreteSignature};
use skull_mir::instance::Instance;
use skull_mir::lower::{ScalarCtx, lower_instance_with_plans};
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::monoty::MonoInterner;

use ustr::Ustr;

/// The stdlib, parsed once and re-checked per call alongside each fixture.
pub static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Run the semantic pass over the stdlib plus `src`.
pub fn ws_of(src: &str) -> bone_core::workspace::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// [`ws_of`] for fixtures that must also type-check cleanly.
pub fn checked_ws_of(src: &str) -> bone_core::workspace::Workspace {
    let ws = ws_of(src);
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}

pub fn decl(ws: &bone_core::workspace::Workspace, name: &str) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str().rsplit("::").next() == Some(name))
        .map(|(fid, _)| fid)
        .unwrap_or_else(|| panic!("no fn `{name}`"));
    FuncRef {
        module: mid,
        func: fid,
    }
}

/// Find the unique scoped definition whose declared name is `name`.
/// Standalone fixtures get a package key that includes their source file, so
/// tests must resolve that key instead of using the bare declaration spelling.
pub fn key(ws: &bone_core::workspace::Workspace, name: &str) -> Ustr {
    let name = Ustr::from(name);
    let all = ws
        .defs
        .iter()
        .filter(|(_, meta)| meta.name == name)
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    let source = all
        .iter()
        .copied()
        .filter(|key| {
            let meta = &ws.defs[key];
            ws.ns
                .packages
                .get(&meta.package)
                .is_some_and(|package| package.self_name.as_str() != "std")
        })
        .collect::<Vec<_>>();
    if source.is_empty() {
        let bare = Ustr::from(name.as_str());
        if ws.struct_by_name.contains_key(&bare)
            || ws.enums_by_name.contains_key(&bare)
            || ws.newtypes_by_name.contains_key(&bare)
            || ws.globals_by_name.contains_key(&bare)
            || ws.functions_by_name.contains_key(&bare)
        {
            return bare;
        }
    }
    let matches = if source.is_empty() { &all } else { &source };
    assert_eq!(
        matches.len(),
        1,
        "expected one definition named `{name}`, found {matches:?}"
    );
    matches[0]
}

pub fn has_name_tail(name: Ustr, declared: &str) -> bool {
    name.as_str().rsplit("::").next() == Some(declared)
}

pub fn lower(source: &str, name: &str) -> MirFunc {
    let ast = skull_parser::parse_module_clean(source, ustr::Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic pass");
    let root = decl(&ws, name);
    let interner = Arc::new(MonoInterner::new());
    let graph =
        skull_graph::instance_graph::build_in(&ws, vec![root], false, Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = ScalarCtx::new(&ws).sharing(interner);
    let plan = &concrete.plans[&Instance::base(root)];
    lower_instance_with_plans(&ws, &scx, plan, &concrete.plans).expect("lower")
}

pub fn signature_plan(instance: Instance, signature: ConcreteSignature) -> ConcreteBodyPlan {
    ConcreteBodyPlan {
        owner: ConcreteOwner::Function(instance.clone()),
        code_identity: Some(skull_mir::code_identity::CodeIdentity::Named(instance)),
        types: Default::default(),
        foreign_domains: Default::default(),
        signature: Arc::new(signature),
        callees: Default::default(),
        failed_sites: Default::default(),
        bound_tree: Default::default(),
    }
}

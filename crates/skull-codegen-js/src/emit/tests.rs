use super::*;

fn workspace(src: &str) -> Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|file| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(file.name).as_str());
            skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("test parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic pass")
}

#[test]
fn a_missing_graph_root_does_not_mint_a_base_instance() {
    let ws = workspace("fn root() {}\n");
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "root")
        .map(|(fid, _)| fid)
        .expect("root");
    let root = FuncRef {
        module: mid,
        func: fid,
    };
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let scx = js_scx(&ws);
    let graph = skull_graph::instance_graph::build_in(&ws, Vec::new(), false, interner.clone());
    let roots = [root];
    let view = artifact::realize(
        &ws,
        &graph,
        &artifact::ArtifactRequest::new(target_projection::JAVASCRIPT_WEB, &roots)
            .with_mappings(artifact::ArtifactMappings::for_backend(
                bone_core::hir::FnBackend::Js,
            ))
            .with_symbols(&artifact::REALIZED_SYMBOLS),
    );

    let concrete = skull_mir::concrete::ConcretePlans::new();
    let always = skull_mir::concrete::ConcreteAlwaysPlans::default();
    let result = WorklistState::new(
        &ws,
        &scx,
        &[(root, "root".to_string())],
        false,
        &graph,
        &view,
        &concrete,
        &always,
        interner,
    );
    let error = match result {
        Ok(_) => panic!("a missing graph root must be rejected"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("root `root` has no base InstanceGraph instance"),
        "missing-root rejection should name the graph invariant: {}",
        error.message
    );
}

#[test]
fn a_missing_inline_publisher_plan_rejects_instead_of_becoming_zero_width() {
    let ws = workspace(
        r#"
inline(js) fn wired_pair(v: float) -> (bool, float) = "return __rock_publish(false, v);"
fn main() {
    let (ok, v) = wired_pair(1.0)
}
"#,
    );
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("test module");
    let main = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .map(|(func, _)| FuncRef { module: mid, func })
        .expect("main");
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let mut graph = skull_graph::instance_graph::build_in(&ws, vec![main], false, interner);
    let before = artifact::realize(
        &ws,
        &graph,
        &artifact::ArtifactRequest::new(target_projection::JAVASCRIPT_WEB, &[main])
            .with_mappings(artifact::ArtifactMappings::for_backend(
                bone_core::hir::FnBackend::Js,
            ))
            .with_symbols(&artifact::REALIZED_SYMBOLS),
    );
    let instance = before
        .publisher_lanes
        .keys()
        .find(|instance| {
            ws.module(instance.func.module).functions[instance.func.func]
                .name
                .as_str()
                == "wired_pair"
        })
        .expect("publisher instance")
        .clone();
    let removed =
        graph.index.subst.remove(&instance).is_some() | graph.root_instances.remove(&instance);
    assert!(
        removed,
        "publisher instance had no graph substitution record"
    );
    let after = artifact::realize(
        &ws,
        &graph,
        &artifact::ArtifactRequest::new(target_projection::JAVASCRIPT_WEB, &[main])
            .with_mappings(artifact::ArtifactMappings::for_backend(
                bone_core::hir::FnBackend::Js,
            ))
            .with_symbols(&artifact::REALIZED_SYMBOLS),
    );
    assert!(after.missing_substitutions.contains(&instance));
    let error = max_published_lanes(
        &after,
        &[JsInlineBody {
            decl: instance.func,
            instance,
        }],
    )
    .expect_err("a missing publisher plan must not become a zero-width answer");
    assert!(
        error.message.contains("no artifact publisher lane plan"),
        "unexpected missing-plan rejection: {}",
        error.message
    );
}

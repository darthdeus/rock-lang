use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

use crate::common::ws_with;

const SOURCE: &str = r#"
value Pair { x: int, y: int }

@scalar_abi inline(js) fn probe<T>(value: T) -> (bool, int) =
    "return __rock_publish(true, 7);"

@nomangle fn root() -> int {
    let (ok_pair, pair_value) = probe(Pair { x: 1, y: 2 })
    let (ok_int, int_value) = probe(3)
    pair_value + int_value
}
"#;

fn root(ws: &bone_core::workspace::Workspace) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "root")
        .map(|(fid, _)| fid)
        .expect("root");
    FuncRef {
        module: mid,
        func: fid,
    }
}

fn definition_headers(js: &str) -> Vec<(String, usize)> {
    js.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("function ")?;
            let (name, args) = rest.split_once('(')?;
            if !name.starts_with("probe_") {
                return None;
            }
            let args = args.split_once(')')?.0.trim();
            let arity = if args.is_empty() {
                0
            } else {
                args.split(',').count()
            };
            Some((name.to_string(), arity))
        })
        .collect()
}

const RUNTIME: &[&str] = &[
    skull_codegen_js::JS_NB_CHECK_SHIMS,
    skull_codegen_js::JS_VEC_CHECK_SHIMS,
    skull_codegen_js::JS_TABLE_PRIM,
    skull_codegen_js::JS_STRUCTURAL_EQ,
    skull_codegen_js::JS_CALL_HELPER,
];

#[test]
fn generic_inline_instances_keep_abi_publishers_calls_and_runtime_together() {
    let ws = ws_with(SOURCE);
    let root = root(&ws);
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .map_err(|e| e.message)
        .expect("generic inline(js) fixture emits");

    let definitions = definition_headers(&js);
    assert_eq!(
        definitions.len(),
        2,
        "the two concrete instantiations must have distinct definitions:\n{js}"
    );
    let mut definition_arities: Vec<_> = definitions.iter().map(|(_, arity)| *arity).collect();
    definition_arities.sort_unstable();
    assert_eq!(
        definition_arities,
        [1, 2],
        "definitions lost instance ABI lanes:\n{js}"
    );
    for (name, _) in &definitions {
        let calls = js.matches(&format!("{name}(")).count();
        assert_eq!(
            calls, 2,
            "{name} must occur once as a definition and once as its matching call target:\n{js}"
        );
    }

    let graph = skull_graph::instance_graph::build(&ws, vec![root], false);
    let roots = [root];
    let view = skull_graph::artifact::realize(
        &ws,
        &graph,
        &skull_graph::artifact::ArtifactRequest::new(
            skull_graph::target_projection::JAVASCRIPT_WEB,
            &roots,
        ),
    );
    let scx = ScalarCtx::new(&ws)
        .with_target(bone_core::hir::FnBackend::Js)
        .sharing(graph.interner().clone());
    let mut parameter_widths = Vec::new();
    let mut publisher_widths = Vec::new();
    for (instance, publisher_lanes) in &view.publisher_lanes {
        let declaration = &ws.module(instance.func.module).functions[instance.func.func];
        if declaration.name.as_str() != "probe" {
            continue;
        }
        let plan = skull_mir::lower::BackendFnAbiPlan::for_boundary_instance(&ws, &scx, instance);
        let symbol = view
            .symbols
            .get(instance)
            .expect("view symbol for probe instance");
        assert!(
            js.contains(&format!("function {symbol}(")),
            "the emitted definition must use the view symbol {symbol}:\n{js}"
        );
        let params = plan.lua_param_names(&ws);
        let header_arity = definitions
            .iter()
            .find(|(name, _)| name == symbol)
            .map(|(_, arity)| *arity)
            .expect("view symbol has an emitted definition");
        let expected_arity = params.as_ref().map_or(1, Vec::len);
        assert_eq!(
            header_arity, expected_arity,
            "definition ABI disagrees with its instance plan"
        );
        assert_eq!(
            *publisher_lanes,
            plan.result_lanes(),
            "publisher lane plan drifted"
        );
        parameter_widths.push(plan.param_leaf_widths().map_or(1, |widths| widths[0]));
        publisher_widths.push(*publisher_lanes);
    }
    parameter_widths.sort_unstable();
    publisher_widths.sort_unstable();
    assert_eq!(
        parameter_widths,
        [1, 2],
        "both instance ABI widths must be retained"
    );
    assert_eq!(
        publisher_widths,
        [2, 2],
        "both instances must publish two lanes"
    );
    assert_eq!(
        publisher_widths.len(),
        2,
        "the probe publisher population is nonzero and exact"
    );
    // Both instance call paths must run.
    crate::common::eval_int(RUNTIME, &js, "root", 14);
}

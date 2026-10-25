use std::sync::Arc;

use skull_mir::instance::Instance;
use skull_mir::lower::{lower_instance_with_plans, ScalarCtx};
use skull_mir::mir::{FuncRef, Inst, MirFunc};
use skull_mir::monoty::MonoInterner;

fn lower(source: &str, name: &str) -> MirFunc {
    let ast = skull_parser::parse_module_clean(source, ustr::Ustr::from("<test>"))
        .expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut vec![ast])
        .expect("semantic pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let fid = ws.module(mid).functions.iter()
        .find(|(_, f)| f.name.as_str() == name).expect("function").0;
    let root = FuncRef { module: mid, func: fid };
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(&ws, vec![root], false, Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = ScalarCtx::new(&ws).sharing(interner);
    let plan = &concrete.plans[&Instance::base(root)];
    lower_instance_with_plans(&ws, &scx, plan, &concrete.plans).expect("lower")
}

fn loads(body: &MirFunc) -> usize {
    body.insts.iter().filter(|(_, inst)| matches!(inst, Inst::Load(_))).count()
}

#[test]
fn immediate_scalar_projection_reads_only_its_parameter_leaf() {
    let body = lower("value Pair { x: int, y: int }\nfn pick(v: Pair) -> int { v.x }", "pick");
    assert_eq!(loads(&body), 1, "raw MIR must not expand unread fields: {body}");
}

#[test]
fn nested_projection_reads_only_its_parameter_subtree() {
    let source = "nobox value Pair { x: int, y: int }\n\
        nobox value Branch { a: Pair, b: Pair, spare: int }\n\
        fn leaf(v: Branch) -> int { v.b.y }\n\
        fn subtree(v: Branch) -> Pair { v.b }\n\
        fn whole(v: Branch) -> Branch { v }";
    let leaf = lower(source, "leaf");
    assert_eq!(loads(&leaf), 1, "nested scalar projection: {leaf}");
    let subtree = lower(source, "subtree");
    assert_eq!(loads(&subtree), 2, "whole selected child needs both leaves: {subtree}");
    let whole = lower(source, "whole");
    assert_eq!(loads(&whole), 5, "whole value still snapshots all leaves: {whole}");
}

#[test]
fn nullable_selected_subtree_keeps_its_presence_leaf() {
    let body = lower("nobox value Pair { x: int, y: int }\n\
        nobox value Branch { spare: int, child: Pair? }\n\
        fn pick(v: Branch) -> Pair? { v.child }", "pick");
    assert_eq!(loads(&body), 3, "selected nullable child needs presence and payload: {body}");
}

fn wide_fields() -> String {
    (0..50).map(|i| format!("f{i}: int")).collect::<Vec<_>>().join(", ")
}

fn fields(body: &MirFunc) -> usize {
    body.insts.iter().filter(|(_, inst)| matches!(inst, Inst::Field { .. })).count()
}

#[test]
fn boxed_wide_parameter_projects_one_leaf_before_copy_expansion() {
    let source = format!("value Wide {{ {} }}\nfn pick(v: Wide) -> int {{ v.f1 }}", wide_fields());
    let body = lower(&source, "pick");
    assert_eq!(fields(&body), 1, "wide parameter primitive demand: {body}");
}

#[test]
fn boxed_wide_local_keeps_initial_snapshot_then_reads_only_demanded_fields() {
    let source = format!("value Wide {{ {} }}\n\
        fn pick(v: Wide) -> (int, int, int) {{ let w = v; (w.f0, w.f1, w.f2) }}", wide_fields());
    let body = lower(&source, "pick");
    assert_eq!(fields(&body), 53, "one full snapshot and three primitive reads: {body}");
}

#[test]
fn boxed_nullable_subtree_reads_its_presence_and_payload_only() {
    let source = format!("nobox value Point {{ x: int, y: int }}\n\
        value Wide {{ {}, child: Point? }}\n\
        fn pick(v: Wide) -> Point? {{ v.child }}", wide_fields());
    let body = lower(&source, "pick");
    assert_eq!(fields(&body), 3, "nullable subtree demand retains presence: {body}");
}

#[test]
fn boxed_global_projects_before_copy_expansion() {
    let source = "value Point { x: int = 1, y: int = 2 }\n\
        value Config { spare: int = 3, child: Point = Point {} }\n\
        global CONFIG: Config = Config {}\n\
        fn leaf() -> int { CONFIG.child.y }\n\
        fn subtree() -> Point { CONFIG.child }\n\
        fn whole() -> Config { CONFIG }";
    let leaf = lower(source, "leaf");
    assert_eq!(fields(&leaf), 2, "only the child and selected leaf: {leaf}");
    let subtree = lower(source, "subtree");
    assert_eq!(fields(&subtree), 3, "selected child snapshots both leaves: {subtree}");
    let whole = lower(source, "whole");
    assert_eq!(fields(&whole), 4, "whole global still snapshots every leaf: {whole}");
}

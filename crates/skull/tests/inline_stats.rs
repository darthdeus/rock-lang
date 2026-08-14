use skull::instance_graph;
use crate::common;

#[test]
fn inline_stats_scans_concrete_body_instances() {
    let ws = common::ws_of(
        r#"
@inline fn ident<T>(x: T) -> T { return x }
@inline fn wrap<T>(x: T) -> T { return ident(x) }

fn main() {
    let a = wrap(1)
    let b = wrap(true)
}
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let mut graph = instance_graph::build(&ws, roots, false);
    let bodies = skull::concrete_body::build(&ws, &mut graph);
    let expected = bodies.plans.len();
    assert!(expected > 3, "fixture did not produce concrete generic instances: {expected}");

    let report = skull::inline_stats::report(&ws);
    assert_eq!(
        report.funcs_scanned, expected,
        "inline stats must scan each concrete body plan, not source declarations"
    );
    assert!(report.total_splices > 0, "concrete plans produced no inline population");
}

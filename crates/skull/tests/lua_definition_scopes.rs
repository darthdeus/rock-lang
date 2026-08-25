use crate::common::{emit_module, run_luajit};
use std::fmt::Write;

#[test]
fn full_emit_partitions_a_large_project_definition_population() {
    let mut source = String::new();
    for i in 0..4_097 {
        writeln!(source, "fn generated_{i}() -> int {{ return {i} }}").unwrap();
    }
    source.push_str("fn main() { print(\"{generated_0() + generated_4096()}\") }\n");

    let lua = emit_module(&source);
    assert_eq!(
        lua.matches("do\n(function()\n").count(),
        2,
        "the full emitter did not partition the definition population"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "definition_scopes"),
        "4096"
    );
}

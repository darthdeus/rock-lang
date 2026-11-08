//! An `Iterator`-bounded body emits STANDALONE to JS at each instantiation.
//!
//! The JS sibling of `rock-lang/crates/skull/tests/bounded_standalone_emission.rs`.
//! JS retains a delivery queue, while the shared graph owns each instance and
//! its resolved bound tree.
//!
use crate::common::ws_with;

fn emit_js(ws: &bone_core::workspace::Workspace) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    skull_codegen_js::emit::emit_program(ws, mid)
        .map(|p| p.program)
        .map_err(|e| e.message)
}

const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

/// The same program with the bound removed. The admission control: it must emit
/// with inlining disabled, so the rejection above is about the BOUND rather than
/// about the mode refusing to emit anything.
const CONCRETE: &str = r#"
fn total(xs: [int]) -> int {
    let s = 0
    for v in xs { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs)}")
}
"#;

/// Three separate things, for the same reason as the luajit sibling: emission
/// succeeding says nothing (a build that dropped `total` also reports no error),
/// a definition existing is checked on the DEFINITION line rather than on the
/// call, and the placeholder having been RESOLVED is checked by the concrete
/// `next` inside that definition -- which is the whole content of D-1.
#[test]
fn a_bounded_body_emits_standalone_to_js_with_inlining_disabled() {
    let _g = crate::common::InlineOffGuard::on();
    let js = emit_js(&ws_with(BOUNDED))
        .expect("a bounded body must emit standalone to JS under its demand");
    let def = js
        .lines()
        .find(|l| l.trim_start().starts_with("function total_") && l.contains("_spec_"))
        .unwrap_or_else(|| panic!("no standalone specialized definition of `total` in:\n{js}"));
    let open = def.find('(').expect("a definition header has params");
    let symbol = def.trim_start()["function ".len()..open - (def.len() - def.trim_start().len())]
        .to_string();
    let body: Vec<&str> = js
        .lines()
        .skip_while(|l| *l != def)
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("function "))
        .collect();
    assert!(
        body.iter().any(|l| l.contains("next_")),
        "the standalone `total` calls no concrete `next`, so its placeholder \
         bound-call was not resolved: {body:?}"
    );
    assert!(
        js.lines()
            .any(|l| !l.trim_start().starts_with("function ") && l.contains(&format!("{symbol}("))),
        "`{symbol}` is emitted but nothing calls it"
    );
}

/// Not inverted: this asserts the CORRECT behavior and must stay green through
/// D-1. Without it the sentinel would be satisfied by the mode rejecting every
/// program, which says nothing about bounded bodies.
#[test]
fn an_unbounded_body_emits_to_js_with_inlining_disabled() {
    let _g = crate::common::InlineOffGuard::on();
    let err = emit_js(&ws_with(CONCRETE)).err();
    assert!(
        err.is_none(),
        "the same program without a bounded generic must still emit to JS with \
         inlining disabled, or the sentinel cannot attribute its rejection to the \
         bound; got {err:?}"
    );
}

#[test]
fn js_specializations_consume_concrete_plan_bound_trees() {
    let source = include_str!("../src/emit.rs");
    assert!(
        !source.contains("graph_trees"),
        "JS must not retain a second graph-tree delivery map"
    );
    assert!(
        source.contains("plan.bound_tree"),
        "JS standalone lowering must read the selected concrete plan tree"
    );
    assert!(
        source.contains("concrete plan has no resolved bound tree"),
        "JS must reject a missing plan tree rather than fabricate an empty one"
    );
}

#[test]
fn a_column_map_initializer_reaches_its_graph_runtime_entry() {
    let source = r#"
nobox value Cell { x: float, y: float }
global CELLS: Map<int, Cell> = map_new()

fn main() {
    CELLS.insert(1, Cell { x: 2.0, y: 3.0 })
}
"#;
    let js = emit_js(&ws_with(source)).expect("column-map initialization must emit");
    assert!(
        js.contains("__map_col_new_spine") && js.contains("new Map()"),
        "the graph must retain the JS map-spine entry reached by the initializer:\n{js}"
    );
}

#[test]
fn a_generic_inline_js_boundary_is_emitted_per_instance() {
    let source = r#"
struct Point { x: int }

fn main() {
    let p = Point { x: 7 }
    print("{field_get(p, .x)}")
}
"#;
    let js = emit_js(&ws_with(source)).expect("the retained field_get boundary must emit");
    assert!(
        js.lines()
            .any(|line| line.starts_with("function field_get_") && line.contains("_spec_")),
        "the concrete inline(js) instance must have its own definition:\n{js}"
    );
}

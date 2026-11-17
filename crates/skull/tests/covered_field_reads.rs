use crate::common::{emit_module, function_body};

const SOURCE: &str = r#"
value Layer { volume: float, speed: float, offset: float, name: string }
struct Step { layers: Vec<Layer> }
global marks: int = 0
fn mark() -> int { marks += 1; return 1 }
@nomangle fn name_total(step: Step) -> int {
    let n = 0
    for layer in step.layers { n += layer.name.len() }
    return n
}
@nomangle fn read_after_call(step: Step) -> int {
    let n = 0
    for layer in step.layers {
        let copy = layer
        n += mark()
        n += copy.name.len()
    }
    return n
}
fn main() {
    let step = Step { layers: [
        Layer { volume: 1.0, speed: 2.0, offset: 3.0, name: "ab" },
        Layer { volume: 4.0, speed: 5.0, offset: 6.0, name: "cde" },
    ] }
    print("{name_total(step)} {read_after_call(step)} {marks}")
}
"#;

/// Copying a value element out reads every field; a later read of the same
/// element raises any error the unused reads could, so those reads go.
#[test]
fn unused_field_reads_before_a_read_of_the_same_base_are_dropped() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "name_total");
    for field in ["volume", "speed", "offset"] {
        assert!(!body.contains(&format!(".{field}")));
    }
}

use crate::common::emit_rejection;

#[test]
fn foreign_bodies_reject_column_maps_in_generic_concrete_and_receiver_slots() {
    for (signature, call) in [
        ("map_probe<V>(m: Map<string, V>)", "map_probe(m)"),
        ("map_probe(m: Map<string, Q>)", "map_probe(m)"),
        ("Map<K, V>.map_probe(m)", "m.map_probe()"),
    ] {
        let source = format!(
            r#"
value Q {{ x: float, y: float }}
@nomangle inline(lua) fn {signature} -> int = "local n = 0; for k, v in pairs(m) do n = n + 1 end; return n"
fn main() {{
    let m: Map<string, Q> = map_new()
    m["a"] = Q {{ x: 1.0, y: 2.0 }}
    print({call})
}}
"#
        );
        let error = emit_rejection(&source);
        assert!(error.contains("column-stored `Map<_, Q>`") && error.contains("map_probe"));
    }
}

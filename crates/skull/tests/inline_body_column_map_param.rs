use crate::common::emit_rejection;

const IMPLICIT_RECEIVER: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn Map<K, V>.probe(map) -> int = """
    local n = 0
    for k, v in pairs(map) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{m.probe()}")
}
"#;

const USER_BODY_GENERIC: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn map_probe<V>(m: Map<string, V>) -> int = """
    local n = 0
    for k, v in pairs(m) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{map_probe(m)}")
}
"#;

const USER_BODY_CONCRETE: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn map_probe(m: Map<string, Q>) -> int = """
    local n = 0
    for k, v in pairs(m) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{map_probe(m)}")
}
"#;

#[test]
fn an_implicit_method_receiver_is_classified() {
    let msg = emit_rejection(IMPLICIT_RECEIVER);
    assert!(msg.contains("column-stored `Map<_, Q>`") && msg.contains("probe"));
}

#[test]
fn a_generic_inline_lua_body_over_a_column_map_rejects() {
    let msg = emit_rejection(USER_BODY_GENERIC);
    assert!(msg.contains("column-stored `Map<_, Q>`") && msg.contains("map_probe"));
}

#[test]
fn a_non_generic_inline_lua_body_over_a_column_map_rejects_identically() {
    assert!(emit_rejection(USER_BODY_CONCRETE).contains("column-stored `Map<_, Q>`"));
}

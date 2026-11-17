use crate::common::emit_rejection;

const GUARDED_GENERIC: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn probe_len<T>(v: [T]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{probe_len(xs)}")
}
"#;

const GUARDED_CONCRETE: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn probe_len(v: [P]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{probe_len(xs)}")
}
"#;

const GUARDED_UNMARKED: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn probe_len<T>(v: [T]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [Q] = [Q { x: 1.0, y: 2.0 }]
    print("{probe_len(xs)}")
}
"#;

const UNGUARDED_GENERIC: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn first_x<T>(v: [T]) -> float = """
    return v._data[1].x
"""

fn main() {
    let xs: [P] = [P { x: 11.0, y: 2.0 }]
    print("{first_x(xs)}")
}
"#;

const GENERIC_PARAM_BOUND_AT_CALL: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn opaque_t<T>(v: T) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{opaque_t(xs)}")
}
"#;

#[test]
fn a_generic_inline_lua_body_over_a_column_vec_rejects() {
    let msg = emit_rejection(GUARDED_GENERIC);
    assert!(msg.contains("column-stored `[P]`") && msg.contains("probe_len"));
}

#[test]
fn a_non_generic_inline_lua_body_over_a_column_vec_rejects_identically() {
    assert!(emit_rejection(GUARDED_CONCRETE).contains("column-stored `[P]`"));
}

#[test]
fn an_unmarked_value_element_rejects_identically() {
    assert!(emit_rejection(GUARDED_UNMARKED).contains("column-stored `[Q]`"));
}

#[test]
fn an_unguarded_inline_lua_body_over_a_column_vec_rejects() {
    assert!(emit_rejection(UNGUARDED_GENERIC).contains("column-stored `[P]`"));
}

#[test]
fn a_generic_param_bound_to_a_column_vec_at_the_call_rejects() {
    let msg = emit_rejection(GENERIC_PARAM_BOUND_AT_CALL);
    assert!(msg.contains("column-stored `[P]`") && msg.contains("opaque_t"));
}

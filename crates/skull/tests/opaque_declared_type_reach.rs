use crate::common::emit_rejection;

const GENERIC_RETURN: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn make<T>() -> T = """
    return {_data = {}, len = 0, __ty = "Vec"}
"""

fn main() {
    let xs: [G] = make()
    print("{xs.len}")
}
"#;

const NAMED_OUT_OF_ORDER: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(token: 0, sink: xs)}")
}
"#;

const POSITIONAL_CONTROL: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs, 0)}")
}
"#;

const DEFAULTED_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int = 0) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs)}")
}
"#;

#[test]
fn a_generic_return_inferred_as_a_column_vec_rejects() {
    let msg = emit_rejection(GENERIC_RETURN);
    assert!(msg.contains("cannot BUILD column-stored `[G]`") && msg.contains("make"));
}

#[test]
fn a_named_out_of_order_column_argument_rejects() {
    let msg = emit_rejection(NAMED_OUT_OF_ORDER);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

#[test]
fn the_positional_form_rejects_too() {
    assert!(emit_rejection(POSITIONAL_CONTROL).contains("column-stored `[G]`"));
}

#[test]
fn a_defaulted_slot_is_reached() {
    let msg = emit_rejection(DEFAULTED_SLOT);
    assert!(msg.contains("column-stored `[G]`"));
}

const IRRELEVANT_UNBOUND_GENERIC: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T, U>(sink: [T]) -> int = """
    return #sink._data
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs)}")
}
"#;

#[test]
fn an_irrelevant_unbound_generic_does_not_disarm_the_rule() {
    let msg = emit_rejection(IRRELEVANT_UNBOUND_GENERIC);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

const NULLABLE_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T]? = null) -> int = """
    if sink == nil then return 0 end
    return #sink._data
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs)}")
}
"#;

const TUPLE_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(pair: ([T], int)) -> int = """
    return #pair[1]._data
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe((xs, 0))}")
}
"#;

#[test]
fn a_nullable_slot_is_reached() {
    let msg = emit_rejection(NULLABLE_SLOT);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

#[test]
fn a_tuple_slot_is_reached() {
    let msg = emit_rejection(TUPLE_SLOT);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

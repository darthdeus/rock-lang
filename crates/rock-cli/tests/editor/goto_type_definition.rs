use super::*;

#[test]
fn type_def_local_binding_to_struct() {
    let source = r#"struct Mob {
    health: int
}

fn spawn() -> Mob {
    return Mob { health: 100 }
}

fn main() {
    let mob = spawn()
}"#;
    let project = Project::single("type_def_local_binding_to_struct", source);
    let reply = project.cli("type-definition", Some((9, 8)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn type_def_variable_use_site() {
    let source = r#"struct Mob {
    health: int
}

fn spawn() -> Mob {
    return Mob { health: 100 }
}

fn main() {
    let mob = spawn()
    let other = mob
}"#;
    let project = Project::single("type_def_variable_use_site", source);
    let reply = project.cli("type-definition", Some((10, 16)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn type_def_field_access_type() {
    let source = r#"struct Inner { value: int }
struct Outer { inner: Inner }

fn main() {
    let o = Outer { inner: Inner { value: 1 } }
    let i = o.inner
}"#;
    let project = Project::single("type_def_field_access_type", source);
    let reply = project.cli("type-definition", Some((5, 14)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn type_def_nullable_peeled_to_struct() {
    let source = r#"struct Mob {
    health: int
}

fn find() -> Mob? {
    return null
}

fn main() {
    let m = find()
}"#;
    let project = Project::single("type_def_nullable_peeled_to_struct", source);
    let reply = project.cli("type-definition", Some((9, 12)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn type_def_enum_value() {
    let source = r#"enum Color { Red, Green, Blue }

fn main() {
    let c = Color::Red
}"#;
    let project = Project::single("type_def_enum_value", source);
    let reply = project.cli("type-definition", Some((3, 8)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 5);
}

#[test]
fn type_def_vec_builtin_jumps_to_stdlib() {
    let source = r#"fn main() {
    let xs = [1, 2, 3]
}"#;
    let project = Project::single("type_def_vec_builtin_jumps_to_stdlib", source);
    let reply = project.cli("type-definition", Some((1, 8)), &[]);
    assert!(array(&reply["definitions"]).len() >= 1, "{reply}");
}

#[test]
fn type_def_primitive_returns_none() {
    let source = r#"fn main() {
    let n = 42
}"#;
    let project = Project::single("type_def_primitive_returns_none", source);
    let reply = project.cli("type-definition", Some((1, 8)), &[]);
    assert!(array(&reply["definitions"]).is_empty(), "{reply}");
}

#[test]
fn type_def_coroutine_jumps_to_yield_type() {
    let source = r#"struct CoFrame { wait: bool }

co fn maker() -> Coroutine<CoFrame> {
    yield CoFrame { wait: true }
}

fn main() {
    let c = maker()
}"#;
    let project = Project::single("type_def_coroutine_jumps_to_yield_type", source);
    let reply = project.cli("type-definition", Some((7, 8)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 0);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn type_def_coroutine_of_primitive_returns_none() {
    let source = r#"co fn nums() -> Coroutine<int> {
    yield 1
}

fn main() {
    let c = nums()
}"#;
    let project = Project::single("type_def_coroutine_of_primitive_returns_none", source);
    let reply = project.cli("type-definition", Some((5, 8)), &[]);
    assert!(array(&reply["definitions"]).is_empty(), "{reply}");
}

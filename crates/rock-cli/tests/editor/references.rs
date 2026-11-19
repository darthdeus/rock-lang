use super::*;

#[test]
fn find_references_local_variable() {
    let source = r#"fn main() {
    let count = 0
    print(count)
    count = count + 1
    if count > 10 {
        print("Done")
    }
}"#;
    let project = Project::single("find_references_local_variable", source);
    let reply = project.cli("references", Some((1, 8)), &[]);
    reference_starts(
        &reply["references"],
        &[(1, 8), (2, 10), (3, 4), (3, 12), (4, 7)],
    );
}

#[test]
fn find_references_function() {
    let source = r#"fn calculate(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let x = calculate(5, 3)
    let y = calculate(10, 20)
    print(calculate(x, y))
}"#;
    let project = Project::single("find_references_function", source);
    let reply = project.cli("references", Some((0, 3)), &[]);
    reference_starts(&reply["references"], &[(0, 3), (5, 12), (6, 12), (7, 10)]);
}

#[test]
fn find_references_function_used_as_bare_value() {
    let source = r#"fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(double, 10)
    print(result)
}"#;
    let project = Project::single("find_references_function_used_as_bare_value", source);
    let reply = project.cli("references", Some((9, 23)), &[]);
    reference_starts(&reply["references"], &[(4, 3), (9, 23)]);
}

#[test]
fn find_references_struct_type() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn distance(p1: Point, p2: Point) -> float {
    return ((p2.x - p1.x) * (p2.x - p1.x) + (p2.y - p1.y) * (p2.y - p1.y)).sqrt()
}

fn main() {
    let origin: Point = Point { x: 0.0, y: 0.0 }
    let p = Point { x: 3.0, y: 4.0 }
}"#;
    let project = Project::single("find_references_struct_type", source);
    let reply = project.cli("references", Some((0, 7)), &[]);
    reference_starts(
        &reply["references"],
        &[(0, 7), (5, 16), (5, 27), (10, 16), (10, 24), (11, 12)],
    );
}

#[test]
fn find_references_struct_field() {
    let source = r#"struct Person {
    name: string,
    age: int,
}

fn main() {
    let p = Person { name: "Alice", age: 30 }
    print(p.name)
    let n = p.name
    if p.age > 18 {
        print("Adult: " + p.name)
    }
}"#;
    let project = Project::single("find_references_struct_field", source);
    let reply = project.cli("references", Some((1, 4)), &[]);
    reference_starts(
        &reply["references"],
        &[(1, 4), (6, 21), (7, 12), (8, 14), (10, 28)],
    );
}

#[test]
fn find_references_parameter() {
    let source = r#"fn greet(name: string, times: int) {
    for i in 0..times {
        print("Hello, " + name)
    }
    if times > 0 {
        print("Greeted " + name + " " + times.to_string() + " times")
    }
}

fn main() {
    greet("World", 3)
}"#;
    let project = Project::single("find_references_parameter", source);
    let reply = project.cli("references", Some((0, 9)), &[]);
    reference_starts(&reply["references"], &[(0, 9), (2, 26), (5, 27)]);
}

#[test]
fn find_references_ufcs_method() {
    let source = r#"fn inc(x: int) -> int { x + 1 }
fn main() {
    let a = inc(5)
    let b = 5.inc()
}"#;
    let project = Project::single("find_references_ufcs_method", source);
    let reply = project.cli("references", Some((3, 14)), &[]);
    reference_starts(&reply["references"], &[(0, 3), (2, 12), (3, 14)]);
}

#[test]
fn find_references_fn_typed_parameter_call() {
    let source = r#"fn apply(g: fn() -> int) -> int {
    g() + g()
}"#;
    let project = Project::single("find_references_fn_typed_parameter_call", source);
    let reply = project.cli("references", Some((1, 10)), &[]);
    reference_starts(&reply["references"], &[(0, 9), (1, 4), (1, 10)]);
}

const FIND_REFERENCES_MACRO_PARAM_IDENT_FRAGMENT: &str = r#"macro decl(Name: ident, Elem: ty) {
    struct Name { v: Elem }
}"#;

#[test]
fn find_references_macro_param_ident_fragment() {
    let source = FIND_REFERENCES_MACRO_PARAM_IDENT_FRAGMENT;
    let project = Project::single("find_references_macro_param_ident_fragment", source);
    let reply = project.cli("references", Some((0, 11)), &[]);
    reference_starts(&reply["references"], &[(0, 11), (1, 11)]);
}

#[test]
fn find_references_macro_param_ty_fragment() {
    let source = FIND_REFERENCES_MACRO_PARAM_IDENT_FRAGMENT;
    let project = Project::single("find_references_macro_param_ty_fragment", source);
    let reply = project.cli("references", Some((0, 24)), &[]);
    reference_starts(&reply["references"], &[(0, 24), (1, 21)]);
}

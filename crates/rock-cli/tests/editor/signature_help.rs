use super::*;

#[test]
fn signature_help_basic_function() {
    let source = r#"fn greet(name: string, times: int) {
    for i in 0..times {
        print("Hello, " + name)
    }
}

fn main() {
    greet("World", 3)
}"#;
    let project = Project::single("signature_help_basic_function", source);
    let reply = project.cli("signature-help", Some((7, 10)), &[]);
    assert!(
        array(&reply["signatures"]).iter().any(
            |signature| string(&signature["label"]).contains("greet(name: string, times: int)")
        ),
        "{reply}"
    );
}

#[test]
fn signature_help_nested_calls() {
    let source = r#"fn add(a: int, b: int) -> int {
    return a + b
}

fn multiply(x: int, y: int) -> int {
    return x * y
}

fn main() {
    let result = multiply(add(2, 3), 4)
}"#;
    let project = Project::single("signature_help_nested_calls", source);
    let reply = project.cli("signature-help", Some((9, 30)), &[]);
    assert!(
        array(&reply["signatures"])
            .iter()
            .any(|signature| string(&signature["label"]).contains("add(a: int, b: int) -> int")),
        "{reply}"
    );
}

#[test]
fn signature_help_overloaded_function() {
    let source = r#"fn process(x: int) -> int {
    return x * 2
}

fn process(x: float) -> float {
    return x * 2.0
}

fn process(x: string) -> string {
    return x + "_processed"
}

fn main() {
    let i = process(42)
    let f = process(3.14)
    let s = process("test")
}"#;
    let project = Project::single("signature_help_overloaded_function", source);
    let reply = project.cli("signature-help", Some((14, 20)), &[]);
    assert!(
        string(
            &reply["signatures"][reply["activeSignature"].as_u64().unwrap_or(0) as usize]["label"]
        )
        .contains("fn process(x: float) -> float"),
        "{reply}"
    );
}

#[test]
fn signature_help_method_call() {
    let source = r#"struct Calculator {
    base: int,
}

// Rock doesn't use impl blocks, methods are defined as regular functions
fn compute(self: Calculator, x: int, y: int) -> int {
    return self.base + x + y
}

fn main() {
    let calc = Calculator { base: 10 }
    let result = compute(calc, 5, 3)
}"#;
    let project = Project::single("signature_help_method_call", source);
    let reply = project.cli("signature-help", Some((11, 30)), &[]);
    assert!(
        array(&reply["signatures"])
            .iter()
            .any(|signature| string(&signature["label"])
                .contains("compute(self: Calculator, x: int, y: int) -> int")),
        "{reply}"
    );
}

#[test]
fn signature_help_generic_function() {
    let source = r#"fn map<T, U>(items: [T], mapper: fn(T) -> U) -> [U] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn main() {
    let numbers = map([1, 2, 3], (x) => x * 2)
}"#;
    let project = Project::single("signature_help_generic_function", source);
    let reply = project.cli("signature-help", Some((9, 22)), &[]);
    assert!(
        array(&reply["signatures"])
            .iter()
            .any(|signature| string(&signature["label"])
                .contains("fn map(items: [T], mapper: fn(T) -> U) -> [U]")),
        "{reply}"
    );
}

#[test]
fn signature_help_constructor() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
}"#;
    let project = Project::single("signature_help_constructor", source);
    let reply = project.cli("signature-help", Some((6, 21)), &[]);
    assert!(
        array(&reply["signatures"])
            .iter()
            .any(|signature| string(&signature["label"]).contains("Point { x: float, y: float }")),
        "{reply}"
    );
}

#[test]
fn signature_help_variadic_function() {
    let source = r#"fn format(template: string, arg: any) -> string {
    // Implementation
    return template
}

fn main() {
    let msg = format("Hello %s", "Alice")
}"#;
    let project = Project::single("signature_help_variadic_function", source);
    let reply = project.cli("signature-help", Some((6, 21)), &[]);
    assert!(
        array(&reply["signatures"])
            .iter()
            .any(|signature| string(&signature["label"])
                .contains("format(template: string, arg: any) -> string")),
        "{reply}"
    );
}

#[test]
fn signature_help_lambda_parameter() {
    let source = r#"fn filter<T>(items: [T], predicate: fn(T) -> bool) -> [T] {
    let result = []
    for item in items {
        if predicate(item) {
            result.push(item)
        }
    }
    return result
}

fn main() {
    let evens = filter([1, 2, 3, 4], (x) => x % 2 == 0)
}"#;
    let project = Project::single("signature_help_lambda_parameter", source);
    let reply = project.cli("signature-help", Some((11, 37)), &[]);
    assert!(
        array(&reply["signatures"])
            .iter()
            .any(|signature| string(&signature["label"])
                .contains("fn filter(items: [T], predicate: fn(T) -> bool) -> [T]")),
        "{reply}"
    );
}

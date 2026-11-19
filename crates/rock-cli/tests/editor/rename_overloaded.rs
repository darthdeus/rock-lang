use super::*;

#[test]
fn rename_overloaded_function_from_definition() {
    let source = r#"fn process(x: int) -> int {
    return x * 2
}

fn process(x: float) -> float {
    return x * 2.0
}

fn process(x: string) -> string {
    return x + "_processed"
}

fn process(x: int, y: int) -> int {
    return x + y
}

fn main() {
    // Test all overloads
    let i = process(42)
    let f = process(3.14)
    let s = process("test")
    let sum = process(10, 20)

    // Multiple calls to same overload
    let i2 = process(100)
    let f2 = process(2.71)
}"#;
    let project = Project::single("rename_overloaded_function_from_definition", source);
    let reply = project.cli("rename", Some((0, 3)), &["transform"]);
    assert!(reply["error"].is_null(), "{reply}");
    contains_text(
        &project.renamed(&reply, None),
        &[
            "transform(x: int) -> int",
            "transform(x: float) -> float",
            "transform(x: string) -> string",
            "transform(x: int, y: int) -> int",
            "transform(42)",
            "transform(3.14)",
            "transform(\"test\")",
            "transform(10, 20)",
            "transform(100)",
            "transform(2.71)",
        ],
    );
}

#[test]
fn rename_overloaded_function_from_call() {
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
    let i = process(42)  // Rename from int overload call
    let f = process(3.14)
    let s = process("test")
}"#;
    let project = Project::single("rename_overloaded_function_from_call", source);
    let reply = project.cli("rename", Some((13, 12)), &["convert"]);
    assert!(reply["error"].is_null(), "{reply}");
    contains_text(
        &project.renamed(&reply, None),
        &[
            "convert(x: int) -> int",
            "convert(x: float) -> float",
            "convert(x: string) -> string",
            "convert(42)",
            "convert(3.14)",
            "convert(\"test\")",
        ],
    );
}

#[test]
fn rename_single_overload_parameter() {
    let source = r#"fn calculate(x: int, y: int) -> int {
    return x + y
}

fn calculate(values: [int]) -> int {
    let sum = 0
    for v in values {
        sum = sum + v
    }
    return sum
}

fn main() {
    let result1 = calculate(5, 10)
    let result2 = calculate([1, 2, 3, 4, 5])
}"#;
    let project = Project::single("rename_single_overload_parameter", source);
    let reply = project.cli("rename", Some((0, 13)), &["a"]);
    assert!(reply["error"].is_null(), "{reply}");
    contains_text(
        &project.renamed(&reply, None),
        &["calculate(a: int, y: int)", "return a + y"],
    );
}

#[test]
fn rename_generic_overloaded_function() {
    let source = r#"fn convert<T>(items: [T], mapper: fn(T) -> T) -> [T] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn convert<K, V>(dict: Map<K, V>, fallback: V) -> V {
    return fallback
}

fn main() {
    let numbers = convert([1, 2, 3], x => x * 2)
    let d: Map<int, int> = map_new()
    let single = convert(d, 0)
}"#;
    let project = Project::single("rename_generic_overloaded_function", source);
    let reply = project.cli("rename", Some((0, 3)), &["transform"]);
    assert!(reply["error"].is_null(), "{reply}");
    contains_text(&project.renamed(&reply, None), &["transform<T>(items: [T]"]);
}

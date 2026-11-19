use super::*;

#[test]
fn rename_across_files_function() {
    let project = Project::package(
        "math.rock",
        &[
            (
                "math.rock",
                r#"fn add(a: int, b: int) -> int {
    return a + b
}

fn multiply(a: int, b: int) -> int {
    return a * b
}"#,
            ),
            (
                "main.rock",
                r#"fn main() {
    let sum = add(5, 3)
    let product = multiply(5, 3)
    print("Sum: " + sum.to_string())
    print("Product: " + product.to_string())
}"#,
            ),
        ],
    );
    let reply = project.cli("rename", Some((0, 3)), &["sum_two_numbers"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, Some("math.rock")).trim_end(),
        (r#"fn sum_two_numbers(a: int, b: int) -> int {
    return a + b
}

fn multiply(a: int, b: int) -> int {
    return a * b
}"#)
        .trim_end()
    );
    assert_eq!(
        project.renamed(&reply, Some("main.rock")).trim_end(),
        (r#"fn main() {
    let sum = sum_two_numbers(5, 3)
    let product = multiply(5, 3)
    print("Sum: " + sum.to_string())
    print("Product: " + product.to_string())
}"#)
        .trim_end()
    );
}

#[test]
fn rename_type_across_modules() {
    let project = Project::package(
        "types.rock",
        &[
            (
                "types.rock",
                r#"struct Point {
    x: float,
    y: float,
}

fn distance(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}"#,
            ),
            (
                "geometry.rock",
                r#"fn create_origin() -> Point {
    return Point { x: 0.0, y: 0.0 }
}

fn create_unit_square() -> [Point] {
    return [
        Point { x: 0.0, y: 0.0 },
        Point { x: 1.0, y: 0.0 },
        Point { x: 1.0, y: 1.0 },
        Point { x: 0.0, y: 1.0 },
    ]
}"#,
            ),
            (
                "main.rock",
                "fn main() {\n    let origin: Point = create_origin()\n    let square = create_unit_square()\n    \n    for p in square {\n        let d = distance(origin, p)\n        print(\"Distance from origin: \" + d.to_string())\n    }\n}",
            ),
        ],
    );
    let reply = project.cli("rename", Some((0, 7)), &["Point2D"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, Some("types.rock")).trim_end(),
        (r#"struct Point2D {
    x: float,
    y: float,
}

fn distance(p1: Point2D, p2: Point2D) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}"#)
        .trim_end()
    );
    assert_eq!(
        project.renamed(&reply, Some("geometry.rock")).trim_end(),
        (r#"fn create_origin() -> Point2D {
    return Point2D { x: 0.0, y: 0.0 }
}

fn create_unit_square() -> [Point2D] {
    return [
        Point2D { x: 0.0, y: 0.0 },
        Point2D { x: 1.0, y: 0.0 },
        Point2D { x: 1.0, y: 1.0 },
        Point2D { x: 0.0, y: 1.0 },
    ]
}"#)
        .trim_end()
    );
    assert_eq!(project.renamed(&reply, Some("main.rock")).trim_end(), ("fn main() {\n    let origin: Point2D = create_origin()\n    let square = create_unit_square()\n    \n    for p in square {\n        let d = distance(origin, p)\n        print(\"Distance from origin: \" + d.to_string())\n    }\n}").trim_end());
}

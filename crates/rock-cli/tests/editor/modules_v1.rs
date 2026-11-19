use super::*;

fn namespace_project() -> Project {
    Project::package(
        "main.rock",
        &[
            (
                "main.rock",
                r#"use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}"#,
            ),
            (
                "shapes.rock",
                r#"pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;"#,
            ),
            (
                "shapes_inner.rock",
                r#"pub mod shapes::inner;



pub fn depth() -> int {
    3
}"#,
            ),
            (
                "facade.rock",
                r#"pub mod facade;

pub use super::shapes::inner::depth;"#,
            ),
        ],
    )
}

#[test]
fn imports_reexports_and_child_modules_across_package_files() {
    let project = namespace_project();
    let definitions = [
        ("alias import", (4, 16), (2, 7)),
        ("glob import", (5, 16), (6, 7)),
        ("re-export", (7, 28), (4, 7)),
    ];
    for (case, at, expected) in definitions {
        assert_eq!(
            project.definition("definition", at),
            Some(expected),
            "{case}"
        );
    }
    let reply = project.cli("references", Some((6, 33)), &[]);
    reference_starts(
        "callable",
        &reply["references"],
        &[(4, 7), (6, 32), (7, 27)],
    );
    let reply = project.cli("completion", Some((8, 32)), &[]);
    completion_labels("child module", &reply, &["depth"]);
}

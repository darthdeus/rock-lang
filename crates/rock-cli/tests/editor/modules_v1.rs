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
fn v1_alias_import_definition() {
    let project = namespace_project();
    let reply = project.cli("definition", Some((4, 16)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 2);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn v1_glob_import_definition() {
    let project = namespace_project();
    let reply = project.cli("definition", Some((5, 16)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 6);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn v1_reexport_definition() {
    let project = namespace_project();
    let reply = project.cli("definition", Some((7, 28)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 4);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}

#[test]
fn v1_callable_references() {
    let project = namespace_project();
    let reply = project.cli("references", Some((6, 33)), &[]);
    reference_starts(&reply["references"], &[(4, 7), (6, 32), (7, 27)]);
}

#[test]
fn v1_child_module_completion() {
    let project = namespace_project();
    let reply = project.cli("completion", Some((8, 32)), &[]);
    completion_labels(&reply, &["depth"]);
}

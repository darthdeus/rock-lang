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

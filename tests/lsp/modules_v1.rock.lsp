The LSP fixture runner creates a named package with the default modules-v1 mode.

TEST: v1_alias_import_definition
TYPE: goto_definition
FILES:
---
FILE: main.rock
---
use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = «cursor»surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}
---
FILE: shapes.rock
---
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;
---
FILE: shapes_inner.rock
---
pub mod shapes::inner;



pub fn depth() -> int {
    3
}
---
FILE: facade.rock
---
pub mod facade;

pub use super::shapes::inner::depth;
---
OPERATION: goto_definition
EXPECTED:
line: 2
character: 7
===

TEST: v1_glob_import_definition
TYPE: goto_definition
SOURCE: v1_alias_import_definition
POSITION: line=5, character=16
EXPECTED:
line: 6
character: 7
===

TEST: v1_reexport_definition
TYPE: goto_definition
SOURCE: v1_alias_import_definition
POSITION: line=7, character=28
EXPECTED:
line: 4
character: 7
===

TEST: v1_callable_references
TYPE: references
SOURCE: v1_alias_import_definition
POSITION: line=6, character=33
EXPECTED:
file: shapes_inner.rock
line: 4
character: 7
file: main.rock
line: 6
character: 32
file: main.rock
line: 7
character: 27
===

TEST: v1_child_module_completion
TYPE: completion
SOURCE: v1_alias_import_definition
POSITION: line=8, character=32
EXPECTED:
- depth
===

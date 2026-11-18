[[tests]]
name = "v1_alias_import_definition"
kind = "goto_definition"
position = [4, 16]
primary_file = "main.rock"
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 2 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests.files]]
path = "main.rock"
text = '''
use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}'''

[[tests.files]]
path = "shapes.rock"
text = '''
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;'''

[[tests.files]]
path = "shapes_inner.rock"
text = '''
pub mod shapes::inner;



pub fn depth() -> int {
    3
}'''

[[tests.files]]
path = "facade.rock"
text = '''
pub mod facade;

pub use super::shapes::inner::depth;'''

[[tests]]
name = "v1_glob_import_definition"
kind = "goto_definition"
position = [5, 16]
primary_file = "main.rock"
source = { from_test = "v1_alias_import_definition" }
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 6 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests.files]]
path = "main.rock"
text = '''
use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}'''

[[tests.files]]
path = "shapes.rock"
text = '''
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;'''

[[tests.files]]
path = "shapes_inner.rock"
text = '''
pub mod shapes::inner;



pub fn depth() -> int {
    3
}'''

[[tests.files]]
path = "facade.rock"
text = '''
pub mod facade;

pub use super::shapes::inner::depth;'''

[[tests]]
name = "v1_reexport_definition"
kind = "goto_definition"
position = [7, 28]
primary_file = "main.rock"
source = { from_test = "v1_alias_import_definition" }
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 4 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests.files]]
path = "main.rock"
text = '''
use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}'''

[[tests.files]]
path = "shapes.rock"
text = '''
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;'''

[[tests.files]]
path = "shapes_inner.rock"
text = '''
pub mod shapes::inner;



pub fn depth() -> int {
    3
}'''

[[tests.files]]
path = "facade.rock"
text = '''
pub mod facade;

pub use super::shapes::inner::depth;'''

[[tests]]
name = "v1_callable_references"
kind = "references"
position = [6, 33]
primary_file = "main.rock"
source = { from_test = "v1_alias_import_definition" }
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 4, character = 7 }, { line = 6, character = 32 }, { line = 7, character = 27 }] }]

[[tests.files]]
path = "main.rock"
text = '''
use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}'''

[[tests.files]]
path = "shapes.rock"
text = '''
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;'''

[[tests.files]]
path = "shapes_inner.rock"
text = '''
pub mod shapes::inner;



pub fn depth() -> int {
    3
}'''

[[tests.files]]
path = "facade.rock"
text = '''
pub mod facade;

pub use super::shapes::inner::depth;'''

[[tests]]
name = "v1_child_module_completion"
kind = "completion"
position = [8, 32]
primary_file = "main.rock"
source = { from_test = "v1_alias_import_definition" }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["depth"] }]

[[tests.files]]
path = "main.rock"
text = '''
use shapes::area as surface;
use shapes::*;

fn main() {
    let alias = surface(2)
    let glob = perimeter(3)
    let nested = shapes::inner::depth()
    let reexport = facade::depth()
    let member = shapes::inner::
    print("{alias} {glob} {nested} {reexport}")
}'''

[[tests.files]]
path = "shapes.rock"
text = '''
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}

pub mod shapes::inner;'''

[[tests.files]]
path = "shapes_inner.rock"
text = '''
pub mod shapes::inner;



pub fn depth() -> int {
    3
}'''

[[tests.files]]
path = "facade.rock"
text = '''
pub mod facade;

pub use super::shapes::inner::depth;'''

TEST: complete_macro_with_qualified_type_shapes
TYPE: completion
---
macro typed_signature(
    a: ref ::demo::Item,
    b: demo::Box<int>,
    c: (ref int)?,
    d: [::demo::Item]
) -> ::demo::Box<int> { 1 }
fn main() {
    typed_
}
---
OPERATION: completion
POSITION: line=7, character=10
---
EXPECTED:
- typed_signature
===

TEST: references_follow_nested_macro_path_patterns
TYPE: references
---
macro choose(Name: ident, value: expr) {
    match value {
        Outer::Wrap(Name::Leaf) => 1,
        Name::Record { child: Name::Leaf } => 2,
        _ => 0
    }
}
fn main() {}
---
OPERATION: references
POSITION: line=0, character=13
---
EXPECTED:
file: main.rock
line: 0
character: 13
file: main.rock
line: 2
character: 20
file: main.rock
line: 3
character: 8
file: main.rock
line: 3
character: 30
===

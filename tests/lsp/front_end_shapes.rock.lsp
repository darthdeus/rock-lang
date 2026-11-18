[[tests]]
name = "complete_macro_with_qualified_type_shapes"
kind = "completion"
position = [7, 10]
source = '''
macro typed_signature(
    a: ref ::demo::Item,
    b: demo::Box<int>,
    c: (ref int)?,
    d: [::demo::Item]
) -> ::demo::Box<int> { 1 }
fn main() {
    typed_
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["typed_signature"] }]

[[tests]]
name = "references_follow_nested_macro_path_patterns"
kind = "references"
position = [0, 13]
source = '''
macro choose(Name: ident, value: expr) {
    match value {
        Outer::Wrap(Name::Leaf) => 1,
        Name::Record { child: Name::Leaf } => 2,
        _ => 0
    }
}
fn main() {}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 13 }, { line = 2, character = 20 }, { line = 3, character = 8 }, { line = 3, character = 30 }] }]

use crate::common::{emit_module, function_body, try_emit_module};

#[test]
fn column_sort_key_uses_leaves_without_boxing_or_row_indices() {
    let lua = emit_module(
        "value V2 { x: float, y: float }
         fn main() {
             let xs = [V2 { x: 2.0, y: 1.0 }, V2 { x: 1.0, y: 2.0 }]
             xs.sort(p => p)
             print(xs[0].x)
         }",
    );
    let bodies: Vec<_> = lua
        .lines()
        .filter(|line| {
            line.starts_with("function ")
                && (line.contains("__lambda") || line.contains("__vec_sort_by_key_leaves"))
        })
        .collect();
    assert!(bodies.len() >= 2);
    for line in bodies {
        let name = line
            .strip_prefix("function ")
            .unwrap()
            .split('(')
            .next()
            .unwrap();
        let body = function_body(&lua, name);
        assert!(!body.contains("V2_ct("));
        assert!(!body.contains('{'));
    }
}

#[test]
fn rejected_sort_key_labels_the_user_call() {
    let simple: Vec<(String, String)> = ["(x > 1, x)", "(x as int?, x)"]
        .into_iter()
        .map(|key| {
            (
                format!("fn main() {{ let xs = [1, 2]\n xs.sort(x => {key}) }}"),
                format!("xs.sort(x => {key})"),
            )
        })
        .collect();
    let cases = simple.iter().map(|(s, c)| (s.as_str(), c.as_str())).chain([
        (
            "value P { x: float, y: float } fn main() { let xs: [P?] = [null]\n xs.sort(p => p) }",
            "xs.sort(p => p)",
        ),
        (
            "fn helper<K>(xs: [int], key: fn(int) -> K) { xs.sort(key) } fn main() { helper([1, 2], x => (x > 1, x)) }",
            "xs.sort(key)",
        ),
        (
            "fn main() { let xs: [int]? = [1, 2]\n xs?.sort(x => (x > 1, x)) }",
            "xs?.sort(x => (x > 1, x))",
        ),
        (
            "struct Key { key_fn: fn(int) -> (bool, int) } fn main() { let xs = [1, 2]\n xs.sort(..Key { key_fn: x => (x > 1, x) }) }",
            "xs.sort(..Key { key_fn: x => (x > 1, x) })",
        ),
    ]);
    for (src, call) in cases {
        let err = try_emit_module(src).expect_err("unordered key must be rejected");
        assert!(err.message.contains("cannot sort by a key"));
        let span = err.labels.first().expect("sort rejection has a label").span;
        assert_eq!(span.file.as_str(), "<test>");
        assert_eq!(&src[span.offset_range().0..span.offset_range().1], call);
    }
}

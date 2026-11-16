use crate::common::{emit_module, function_body, try_emit_module};

#[test]
fn column_sort_key_takes_value_leaves_without_boxing() {
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
    assert!(bodies.len() >= 2, "fixture must emit the key and leaf sort");
    for line in bodies {
        let name = line
            .strip_prefix("function ")
            .unwrap()
            .split('(')
            .next()
            .unwrap();
        let body = function_body(&lua, name);
        assert!(!body.contains("V2_ct("), "sort must not box a row: {body}");
    }
}

#[test]
fn wide_sort_does_not_allocate_row_indices() {
    let lua = emit_module(
        "value V2 { x: float, y: float }
         fn main() {
             let xs = [V2 { x: 2.0, y: 1.0 }, V2 { x: 1.0, y: 2.0 }]
             xs.sort(p => p)
             print(xs[0].x)
         }",
    );
    assert!(
        !lua.contains("= __sort_rows_int_fn_int_int_ret_bool("),
        "wide sort must not allocate row indices"
    );
}

#[test]
fn rejected_sort_key_labels_the_user_call() {
    for key in ["(x > 1, x)", "(x as int?, x)"] {
        let src = format!("fn main() {{ let xs = [1, 2]\n xs.sort(x => {key}) }}");
        let err = try_emit_module(&src).expect_err("unordered key must be rejected");
        assert!(err.message.contains("cannot sort by a key"), "{err:?}");
        let span = err.labels.first().expect("sort rejection has a label").span;
        assert_eq!(span.file.as_str(), "<test>");
        assert_eq!(
            &src[span.offset_range().0..span.offset_range().1],
            format!("xs.sort(x => {key})")
        );
    }
    for (src, call) in [
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
    ] {
        let err = try_emit_module(src).expect_err("unordered key must be rejected");
        assert!(err.message.contains("cannot sort by a key"), "{err:?}");
        let span = err.labels.first().expect("sort rejection has a label").span;
        assert_eq!(span.file.as_str(), "<test>");
        assert_eq!(&src[span.offset_range().0..span.offset_range().1], call);
    }
}

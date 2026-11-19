use crate::common::compile_and_run;

#[test]
fn user_names_shaped_like_emitter_locals_keep_their_values() {
    for (src, expected) in [
        (
            "global r: int = 1\nfn use_it(a: int) -> int { return a + r }\nfn main() { print(\"{use_it(1)}\") }",
            "2",
        ),
        (
            "global rx1: int = 1\nfn use_it(a: int) -> int { return a + rx1 }\nfn main() { print(\"{use_it(1)}\") }",
            "2",
        ),
        (
            "global r1x: int = 1\nfn use_it(a: int) -> int { return a + r1x }\nfn main() { print(\"{use_it(1)}\") }",
            "2",
        ),
        (
            "fn radius(a: int) -> int { return a * 2 }\nfn main() { print(\"{radius(3)}\") }",
            "6",
        ),
        (
            "global __rock_foo: int = 9\nfn use_it(a: int) -> int { return a + __rock_foo }\nfn main() { print(\"{use_it(1)}\") }",
            "10",
        ),
    ] {
        assert_eq!(compile_and_run(src), expected, "{src}");
    }
}

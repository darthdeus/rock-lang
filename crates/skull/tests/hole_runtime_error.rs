use crate::common::{emit_module, function_body};

#[test]
fn holes_emit_a_runtime_error_and_keep_the_function() {
    for (src, message) in [
        (
            "struct P { x: int, y: int }\nfn boom() -> int {\n    let p = P { x: 1, y: ? }\n    return p.y\n}\nfn main() {}\n",
            "unfilled hole",
        ),
        (
            "fn boom() {\n    let x: int = ?\n    print(\"{x}\")\n}\nfn main() {}\n",
            "unfilled hole",
        ),
        (
            "fn boom() -> int {\n    return ?width\n}\nfn main() {}\n",
            "?width",
        ),
    ] {
        let lua = emit_module(src);
        let body = function_body(&lua, "boom");
        assert!(body.contains("error(") && body.contains(message), "{body}");
    }
}

use std::fs;
use std::path::Path;

fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("missing constructor {name}"));
    let tail = &source[start..];
    let end = tail[3..]
        .find("\nfn ")
        .map(|offset| offset + 3)
        .unwrap_or(tail.len());
    &tail[..end]
}

#[test]
fn canvas_constructors_refuse_zero_backend_handles() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("../packages/stone/engine/bindings.rock"))
        .expect("read canvas bindings");
    for name in ["new_canvas", "new_canvas_with_depth"] {
        let body = function_body(&source, name);
        assert!(
            body.contains("if handle == 0"),
            "{name} must refuse a zero backend handle before returning Canvas"
        );
        assert!(
            body.contains("error("),
            "{name} must report a zero backend handle instead of wrapping it"
        );
    }
}

use crate::common::emit_rejection;

#[test]
fn foreign_bodies_reject_flat_vec_arguments_regardless_of_body_or_signature() {
    for (mark, signature, body) in [
        (
            "nobox",
            "probe<T>(v: [T])",
            "if v._data == nil then return -1 end; return #v._data",
        ),
        ("nobox", "probe(v: [P])", "return #v._data"),
        ("", "probe<T>(v: [T])", "return #v._data"),
        ("nobox", "probe<T>(v: T)", "return #v._data"),
    ] {
        let source = format!(
            r#"
{mark} value P {{ x: float, y: float }}
@nomangle inline(lua) fn {signature} -> int = "{body}"
fn main() {{
    let xs: [P] = [P {{ x: 1.0, y: 2.0 }}]
    print(probe(xs))
}}
"#
        );
        let error = emit_rejection(&source);
        assert!(error.contains("column-stored `[P]`") && error.contains("probe"));
    }
}

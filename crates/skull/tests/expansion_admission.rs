use crate::common::{module_id, ws_with_stdlib};

#[test]
fn direct_admission_rejects_invalid_vertex_expansion() {
    let ws = ws_with_stdlib(
        r#"
@vertex_expand inline(lua) fn stride<T>(u: T) -> float = """
    error("stride must be compiler-expanded")
"""
fn main() { let _ = stride(3) }
"#,
    );
    let mid = module_id(&ws, "<test>");
    let err = skull_mir::lower::expansion_rejection(&ws, mid)
        .expect("fixture must be rejected by admission");
    assert!(err.message.contains("`@vertex_expand`"));
}

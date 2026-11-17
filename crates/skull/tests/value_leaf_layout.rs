const MUTATION_PROGRAM: &str = r#"
nobox value Inner { a: float, b: int }
nobox value Outer { inner: Inner, c: float, e: float, d: int }
fn main() {
    let o = Outer { inner: Inner { a: 1.0, b: 2 }, c: 3.0, e: 5.0, d: 4 }
    print(o.inner.b + o.d)
}
"#;

fn assert_load_fails(lua: &str, edits: &[(&str, &str)], message: &str) {
    let mut edited = lua.to_string();
    for (from, to) in edits {
        assert_eq!(edited.matches(from).count(), 1);
        edited = edited.replace(from, to);
    }
    let (ok, out) = crate::common::run_luajit_capturing(&edited);
    assert!(!ok && out.contains(message));
}

#[test]
fn emitted_layout_checks_reject_offset_and_size_drift() {
    let lua = crate::common::emit_module(MUTATION_PROGRAM);
    assert_eq!(crate::common::run_main(&lua), "6");
    for (from, to, message) in [
        (
            "  double c;\n  double e;\n",
            "  double e;\n  double c;\n",
            "skull layout drift: Outer.c at 24, expected 16",
        ),
        (
            "double __rock_leaf_696e6e65722e61; int __rock_leaf_696e6e65722e62;",
            "int __rock_leaf_696e6e65722e62; double __rock_leaf_696e6e65722e61;",
            "skull leaf alias drift: Outer.__rock_leaf_696e6e65722e61",
        ),
        (
            "  int d;\n} Outer;",
            "  int d;\n  double pad;\n} Outer;",
            "skull layout drift: sizeof(Outer) = 48, expected 40",
        ),
    ] {
        assert_load_fails(&lua, &[(from, to)], message);
    }
}

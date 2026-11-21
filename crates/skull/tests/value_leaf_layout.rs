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

use crate::common::compile_and_run;

const SRC: &str = r#"
fn seed<T>() -> T? { return null }

struct Box<T> { value: T? = seed() }

fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#;

#[test]
fn generic_defaults_use_the_concrete_instance() {
    assert_eq!(compile_and_run(SRC), "nil nil");
}

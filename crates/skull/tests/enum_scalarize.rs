use crate::common::bone_rejections;

#[test]
fn record_variant_pattern_must_name_the_scrutinee_enum() {
    const SRC: &str = r#"
enum Choice { A { left: int, shared: int }, B { right: string, shared: string } }
enum Other { A { ghost: int } }
fn read() -> int {
    return match (Choice::A { left: 7, shared: 13 }) {
        Other::A { ghost } => ghost,
        Choice::A { left } => left,
        Choice::B { right } => right.len(),
    }
}
fn main() { print(read()) }
"#;

    assert_eq!(
        bone_rejections(SRC),
        ["pattern for enum `Other` cannot match a value of type `Choice`"]
    );
}

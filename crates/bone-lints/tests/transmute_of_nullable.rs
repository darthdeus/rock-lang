use crate::support::{lint_only, quoted};
use bone_lints::{Finding, TRANSMUTE_OF_NULLABLE};
use rock_core::errors::CompilerErrorLevel;

const PRELUDE: &str = r#"
inline(lua) fn cast<T>(value: any) -> T = "return value"
inline(lua) fn transmute<A, B>(x: A) -> B = "return x"

fn maybe_int() -> int? {
    return 1
}

fn always_int() -> int {
    return 1
}
"#;

fn findings(body: &str) -> Vec<Finding> {
    let src = format!("{PRELUDE}\n{body}\n");
    lint_only(&[("<transmute_of_nullable>", &src)], TRANSMUTE_OF_NULLABLE)
}

#[test]
fn nullability_diagnostics() {
    for (body, expected) in [
        ("let _x: int = transmute(maybe_int())", vec!["transmute"]),
        ("let _x: int = cast(maybe_int())", vec!["cast"]),
        ("let _x: int? = cast(maybe_int())", vec![]),
        ("let _x: int = cast(always_int())", vec![]),
        ("let _x: int = transmute(always_int())", vec![]),
        (
            "let inner: int? = cast(maybe_int())\nlet _x: int = cast(inner)",
            vec!["cast"],
        ),
        (
            "// rock: allow(transmute_of_nullable)\nlet _x: int = cast(maybe_int())",
            vec![],
        ),
        (
            "let _x: int = cast(maybe_int()) // rock: allow(transmute_of_nullable)",
            vec![],
        ),
    ] {
        let fs = findings(&format!("fn main() {{\n{body}\n}}"));
        assert_eq!(
            fs.iter().map(quoted).collect::<Vec<_>>(),
            expected,
            "{body}"
        );
        assert!(fs.iter().all(|f| f.level() == CompilerErrorLevel::Error));
    }
    let fs = findings(
        "struct Mob { hp: int }\nfn maybe_mob() -> Mob? { Mob { hp: 10 } }\nfn main() { let _m: Mob = transmute(maybe_mob()) }",
    );
    assert_eq!(fs.len(), 1);
    assert!(fs[0].error.message.contains("from a `Mob?`"));
}

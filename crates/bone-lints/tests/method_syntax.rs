use bone_lints::{LintOptions, lint_workspace};
use bone_semantic::prelude::*;

const FILE: &str = "<method_syntax>";
const MOB: &str = "struct Mob { hp: int }\n";
const DAMAGE: &str = "fn damage(m: Mob, n: int) { m.hp -= n }\n";

fn messages(src: &str, opts: &LintOptions) -> Vec<String> {
    crate::lint(&[(FILE, src)], opts, true)
        .into_iter()
        .map(|f| f.error.message)
        .collect()
}

fn selected() -> LintOptions {
    LintOptions::only(["method_syntax".to_owned()])
}

#[test]
fn receiver_suggestions() {
    for (decl, function, suggestion) in [
        (MOB, DAMAGE, "fn Mob.damage(m, n: int)"),
        (
            "enum Phase { Idle, Busy }",
            "fn describe(p: Phase, n: int) -> int { n }",
            "fn Phase.describe(p, n: int)",
        ),
        (
            "newtype Hp(int)",
            "fn describe(p: Hp, n: int) -> int { n }",
            "fn Hp.describe(p, n: int)",
        ),
        (
            MOB,
            "fn damage(m: Mob, n: int = 3) -> int { n }",
            "fn Mob.damage(m, ..)",
        ),
    ] {
        let src = format!("{decl}\n{function}\nfn main() {{}}");
        let msgs = messages(&src, &selected());
        assert_eq!(msgs.len(), 1, "{src}: {msgs:?}");
        assert!(msgs[0].contains(suggestion), "{}", msgs[0]);
    }
}

#[test]
fn exempt_signatures() {
    for function in [
        "fn Mob.damage(m, n: int) { m.hp -= n }",
        "extern(lua) fn take(m: Mob)",
        "fn f(x: int, m: Mob) {}",
        "fn f(xs: Vec<Mob>) {}",
        "fn f(m: Mob?) {}",
        "fn f<T>(t: T, m: Mob) {}",
        "fn f(t: (Mob, int)) {}",
        "fn f() {}",
        "@shader(fragment) fn shade(input: Mob, n: int) -> float { 1.0 }",
    ] {
        let src = format!("{MOB}{function}\nfn main() {{}}");
        assert!(messages(&src, &selected()).is_empty(), "{src}");
    }
}

#[test]
fn selection_and_type_filters() {
    let src = format!(
        "{MOB}struct Color {{ r: int }}\n{DAMAGE}fn tint(c: Color, n: int) -> int {{ n }}\nfn main() {{}}"
    );
    assert!(
        crate::lint(&[(FILE, &src)], &Default::default(), true)
            .iter()
            .all(|f| f.lint != "method_syntax")
    );
    for (opts, expected) in [
        (selected(), vec!["Mob.damage", "Color.tint"]),
        (
            selected().ignoring_types([Ustr::from("Color")]),
            vec!["Mob.damage"],
        ),
        (
            selected().ignoring_types([Ustr::from("Color"), Ustr::from("Mob")]),
            vec![],
        ),
        (
            selected().over_types([Ustr::from("Color")]),
            vec!["Color.tint"],
        ),
        (
            selected()
                .over_types([Ustr::from("Color"), Ustr::from("Mob")])
                .ignoring_types([Ustr::from("Color")]),
            vec!["Mob.damage"],
        ),
    ] {
        let msgs = messages(&src, &opts);
        assert_eq!(msgs.len(), expected.len(), "{msgs:?}");
        for name in expected {
            assert!(msgs.iter().any(|m| m.contains(name)), "{name}: {msgs:?}");
        }
    }
    let src = "fn main() { let unread = 5 }";
    assert!(messages(src, &selected()).is_empty());
    assert_eq!(messages(src, &LintOptions::default()).len(), 1);
    assert_eq!(
        LintOptions::only(["method_sytnax".to_owned(), "unused_var".to_owned()]).unknown(),
        ["method_sytnax"]
    );
}

#[test]
fn receiver_types_in_warm_modules_and_unknown_names() {
    let game = "fn draw(s: Sprite, n: int) -> int { n }\nfn main() {}";
    let mut asts = [
        crate::support::parse("<lib>", "struct Sprite { id: int }"),
        crate::support::parse(FILE, game),
    ];
    let ws = semantic_pass(&mut asts).expect("typechecks");
    let sources = [(Ustr::from(FILE), game)].into_iter().collect();
    assert_eq!(
        lint_workspace(&ws, &FxHashSet::default(), true, &sources, &selected()).len(),
        1
    );
    let warm = ws
        .modules
        .iter()
        .filter(|m| m.file.as_str() == "<lib>")
        .map(|m| m.id)
        .collect();
    assert!(lint_workspace(&ws, &warm, true, &sources, &selected()).is_empty());
    assert_eq!(
        bone_lints::unknown_type_names(
            &ws,
            &FxHashSet::default(),
            &[Ustr::from("Sprite"), Ustr::from("Colour")]
        ),
        [Ustr::from("Colour")]
    );
}

//! `method_syntax`: a free function whose first param is a user type, written
//! in prefix form (`fn damage(m: Mob, n: int)`) rather than receiver form
//! (`fn Mob.damage(m, n: int)`). Opt-in, so a project can be converted a file at
//! a time instead of in one sweep. Harness mirrors catalog.rs.

use bone_lints::{Finding, LintOptions, lint_workspace};
use bone_semantic::prelude::*;

const FILE: &str = "<method_syntax>";

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse")
}

fn findings_with(src: &str, sel: &LintOptions) -> Vec<Finding> {
    let ws = semantic_pass(&mut [parse(src)]).expect("clean program typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), src);
    lint_workspace(&ws, &FxHashSet::default(), true, &sources, sel)
}

/// The lint on its own: `--lints=method_syntax`, so no other lint's findings
/// can be mistaken for this one's.
fn methods(src: &str) -> Vec<String> {
    methods_ignoring(src, [])
}

fn methods_ignoring(src: &str, ignore: impl IntoIterator<Item = Ustr>) -> Vec<String> {
    let opts = LintOptions::only(["method_syntax".to_string()]).ignoring_types(ignore);
    findings_with(src, &opts).iter().map(|f| f.error.message.clone()).collect()
}

const MOB: &str = "struct Mob { hp: int }\n";
const DAMAGE: &str = "fn damage(m: Mob, n: int) {\n    m.hp -= n\n}\n";
const CALL_MAIN: &str = "fn main() { damage(Mob { hp: 1 }, 1) }\n";

#[test]
fn flags_a_prefix_fn_over_a_user_struct() {
    let src = format!("{MOB}{DAMAGE}{CALL_MAIN}");
    let msgs = methods(&src);
    assert_eq!(msgs.len(), 1, "one candidate, got {msgs:?}");
    assert!(msgs[0].contains("fn Mob.damage(m, n: int)"), "{}", msgs[0]);
}

#[test]
fn a_receiver_form_decl_is_already_converted() {
    // The discriminant this lint turns on: same HIR, different surface. If it
    // reads only the lowered signature it flags every already-converted fn.
    let src = format!(
        "{MOB}fn Mob.damage(m, n: int) {{\n    m.hp -= n\n}}\nfn main() {{ Mob {{ hp: 1 }}.damage(1) }}\n"
    );
    assert!(methods(&src).is_empty(), "{:?}", methods(&src));
}

#[test]
fn skips_a_first_param_that_cannot_be_a_receiver_path() {
    // Builtins, containers, nullables, generics and tuples have no `Type.name`
    // spelling; flagging them would suggest an edit that does not compile.
    for sig in [
        "fn f(x: int, m: Mob)",
        "fn f(xs: Vec<Mob>)",
        "fn f(m: Mob?)",
        "fn f<T>(t: T, m: Mob)",
        "fn f(t: (Mob, int))",
        "fn f()",
    ] {
        let src = format!("{MOB}{sig} {{ }}\nfn main() {{ }}\n");
        assert!(methods(&src).is_empty(), "{sig} must not be flagged: {:?}", methods(&src));
    }
}

#[test]
fn an_extern_decl_is_never_flagged() {
    // The parser REJECTS a receiver path on an extern decl, so the suggested
    // rewrite would not compile.
    let src = format!("{MOB}extern fn take(m: Mob)\nfn main() {{ take(Mob {{ hp: 1 }}) }}");
    assert!(methods(&src).is_empty(), "{:?}", methods(&src));
}

#[test]
fn suppressed_by_an_allow_comment() {
    let src = format!("{MOB}// rock: allow(method_syntax)\n{DAMAGE}{CALL_MAIN}");
    assert!(methods(&src).is_empty(), "{:?}", methods(&src));
}

#[test]
fn off_unless_the_invocation_asks_for_it() {
    // The point of the opt-in tier: a migration lint fires on nearly every
    // existing declaration, so a build, the LSP, and a bare `rock lint` must
    // not carry it.
    let src = format!("{MOB}{DAMAGE}{CALL_MAIN}");
    let default_tier = findings_with(&src, &LintOptions::default());
    assert!(
        default_tier.iter().all(|f| f.lint != "method_syntax"),
        "{:?}",
        default_tier.iter().map(|f| f.lint).collect::<Vec<_>>()
    );
}

#[test]
fn an_explicit_selection_runs_only_the_named_lints() {
    // `--lints=` is a focus flag: naming one lint silences the rest, which is
    // what makes a migration sweep readable.
    let src = "fn main() {\n    let unread = 5\n}";
    let sel = LintOptions::only(["method_syntax".to_string()]);
    assert!(findings_with(src, &sel).is_empty());
    let default_tier = findings_with(src, &LintOptions::default());
    assert_eq!(default_tier.len(), 1, "unused_var still fires by default");
}

#[test]
fn an_enum_or_newtype_receiver_is_a_candidate_too() {
    // `Type.name` spells any nominal decl, so the candidate set is
    // structs + enums + newtypes, not structs alone.
    for decl in ["enum Phase { Idle, Busy }", "newtype Hp(int)"] {
        let ty = if decl.starts_with("enum") { "Phase" } else { "Hp" };
        let src = format!("{decl}\nfn describe(p: {ty}, n: int) -> int {{ n }}\nfn main() {{ }}\n");
        let msgs = methods(&src);
        assert_eq!(msgs.len(), 1, "{decl}: {msgs:?}");
        assert!(msgs[0].contains(&format!("fn {ty}.describe(p, n: int)")), "{}", msgs[0]);
    }
}

#[test]
fn a_default_valued_param_is_not_spelled_out() {
    // The signature does not carry the default's expression, so the suggestion
    // must not render a param list that drops it.
    let src = format!("{MOB}fn damage(m: Mob, n: int = 3) -> int {{ n }}\nfn main() {{ }}\n");
    let msgs = methods(&src);
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert!(msgs[0].contains("fn Mob.damage(m, ..)"), "{}", msgs[0]);
}

#[test]
fn a_receiver_declared_in_a_warm_module_is_not_the_authors_to_convert() {
    // The user-type gate. A stdlib type is a read-only reference here, so a
    // fn over one is not a migration candidate -- and without the gate every
    // `fn f(v: Vec2)` in a game would be reported.
    let lib = "struct Sprite { id: int }\n";
    let game = "fn draw(s: Sprite, n: int) -> int { n }\nfn main() { }\n";
    let mut asts = [
        skull_parser::parse_module_clean(lib, Ustr::from("<lib>")).expect("parse"),
        skull_parser::parse_module_clean(game, Ustr::from(FILE)).expect("parse"),
    ];
    let ws = semantic_pass(&mut asts).expect("typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(FILE), game);
    let sel = LintOptions::only(["method_syntax".to_string()]);
    let cold = lint_workspace(&ws, &FxHashSet::default(), true, &sources, &sel);
    assert_eq!(cold.len(), 1, "both modules cold: Sprite is a user type -- {cold:?}");

    let warm: FxHashSet<ModuleId> = ws
        .modules
        .iter()
        .filter(|m| m.file.as_str() == "<lib>")
        .map(|m| m.id)
        .collect();
    assert_eq!(warm.len(), 1, "one warm module");
    assert!(lint_workspace(&ws, &warm, true, &sources, &sel).is_empty());
}

#[test]
fn an_ignored_receiver_type_drops_out_of_the_sweep() {
    // The iteration knob: a project converts by type, so the types already
    // handled (or deliberately left alone) come off the list without touching
    // the source.
    let src = format!(
        "{MOB}struct Color {{ r: int }}\n{DAMAGE}fn tint(c: Color, n: int) -> int {{ n }}\n{CALL_MAIN}"
    );
    let all = methods_ignoring(&src, []);
    assert_eq!(all.len(), 2, "both candidates without an ignore list: {all:?}");

    let kept = methods_ignoring(&src, [Ustr::from("Color")]);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].contains("fn Mob.damage"), "{}", kept[0]);

    let none = methods_ignoring(&src, [Ustr::from("Color"), Ustr::from("Mob")]);
    assert!(none.is_empty(), "{none:?}");
}

#[test]
fn an_ignored_name_that_is_no_type_is_reported_back() {
    // A typo'd ignore silently fails OPEN (the type keeps showing up), which
    // reads as the flag not working. Name it instead.
    let src = format!("{MOB}{DAMAGE}{CALL_MAIN}");
    let ws = semantic_pass(&mut [parse(&src)]).expect("typechecks");
    let names = [Ustr::from("Mob"), Ustr::from("Colour")];
    assert_eq!(
        bone_lints::unknown_type_names(&ws, &FxHashSet::default(), &names),
        vec![Ustr::from("Colour")]
    );
}

#[test]
fn unknown_lint_names_are_reported_back() {
    let sel = LintOptions::only(["method_sytnax".to_string(), "unused_var".to_string()]);
    assert_eq!(sel.unknown(), vec!["method_sytnax"]);
}

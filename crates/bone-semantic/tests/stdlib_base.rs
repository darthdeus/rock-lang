//! The batch semantic pass reuses the stdlib checked once per process when no
//! user module can change it, and that reuse is invisible in the result.

use std::sync::atomic::Ordering;

use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::{semantic_pass_into, semantic_pass_without_closure_gate};
use skull_parser::Ast;

fn with_stdlib(user: &str) -> Vec<Ast> {
    let mut modules = skull_parser::stdlib::modules().expect("stdlib parses");
    modules.push(skull_parser::parse_module(user, Ustr::from("<test>")).expect("user parses"));
    modules
}

fn reused(ws: &Workspace) -> bool {
    // A pass from the base lowers only the user module's bodies.
    ws.relowered_body_count.load(Ordering::Relaxed) < 50
}

/// What lowering and inference recorded, rendered per module. Scope probes
/// are left out: they record what a lowering asked, for the session.
fn render(ws: &Workspace) -> Vec<String> {
    let mut out: Vec<String> = ws
        .modules
        .iter()
        .map(|m| {
            format!(
                "{}: fns {:?} structs {:?} enums {:?} globals {:?} bodies {:?} lowering {:?} interface {:?} inference {:?}",
                m.file,
                m.functions,
                m.structs,
                m.enums,
                m.globals,
                sorted(m.bodies.iter().map(|(k, v)| format!("{k:?}={v:?}"))),
                sorted(m.lowering_errors.iter().map(|(k, v)| format!("{k:?}={v:?}"))),
                m.interface_errors,
                ws.inference(m.id).map(|inf| {
                    sorted(inf.errors.iter().map(|(k, v)| format!("{k:?}={v:?}")))
                }),
            )
        })
        .collect();
    out.push(format!(
        "{:?}",
        sorted(ws.module_ids.iter().map(|(k, v)| format!("{k}={v:?}")))
    ));
    out
}

fn sorted(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut items: Vec<String> = items.collect();
    items.sort();
    items
}

/// Check `user` both ways; return whether the base served it.
fn assert_same_as_combined(user: &str) -> bool {
    let mut combined = with_stdlib(user);
    let expected = semantic_pass_into(Workspace::default(), &mut combined, &Default::default())
        .map(|ws| render(&ws))
        .map_err(|e| e.message);
    let mut modules = with_stdlib(user);
    let pass = semantic_pass_without_closure_gate(&mut modules, &Default::default());
    let was_reused = pass.as_ref().is_ok_and(reused);
    let actual = pass.map(|ws| render(&ws)).map_err(|e| e.message);
    match (&expected, &actual) {
        (Ok(e), Ok(a)) => {
            assert_eq!(e.len(), a.len());
            for (e, a) in e.iter().zip(a) {
                assert_eq!(e, a, "for user module:\n{user}");
            }
        }
        _ => assert_eq!(expected, actual, "for user module:\n{user}"),
    }
    was_reused
}

#[test]
fn a_user_module_apart_from_the_stdlib_reuses_it() {
    let mut modules = with_stdlib("fn main() -> int { 1 + 2 }\n");
    let ws = semantic_pass_without_closure_gate(&mut modules, &Default::default()).unwrap();
    assert_eq!(ws.relowered_body_count.load(Ordering::Relaxed), 1);
}

#[test]
fn reuse_matches_a_combined_pass() {
    let programs = [
        "fn main() -> int { 1 + 2 }\n",
        "fn main() { let v = vec2(1.0, 2.0); let w = v + v; print(w.x) }\n",
        "fn main() { let xs = [1, 2, 3]; for x in xs { print(x) } }\n",
        "fn main() -> int { \"not an int\" }\n",
        "struct P { x: int }\nfn main() -> int { let p = P { x: 1 }; p.y }\n",
        "fn apply<T>(x: T, f: fn(T) -> T) -> T { f(x) }\nfn main() -> int { apply(1, |x| x + 1) }\n",
    ];
    for program in programs {
        assert!(assert_same_as_combined(program), "not reused:\n{program}");
    }
}

#[test]
fn a_user_module_that_can_reach_the_stdlib_lowers_with_it() {
    let programs = [
        // A user definition under a stdlib name joins or collides with it.
        "fn print(x: int) {}\nfn main() { print(1) }\n",
        "struct Vec2 { x: int }\nfn main() {}\n",
        // A user macro could rewrite a stdlib call.
        "macro twice(x: int) -> int { x + x }\nfn main() -> int { twice(2) }\n",
    ];
    for program in programs {
        assert!(!assert_same_as_combined(program), "reused:\n{program}");
    }
}

#[test]
fn an_edited_stdlib_prefix_is_not_reused() {
    let mut modules = with_stdlib("fn main() {}\n");
    modules[0].items.pop();
    assert!(!skull_parser::stdlib::is_prefix_of(&modules));
    let mut modules = with_stdlib("fn main() {}\n");
    modules.swap(0, 1);
    assert!(!skull_parser::stdlib::is_prefix_of(&modules));
    assert!(skull_parser::stdlib::is_prefix_of(&with_stdlib(
        "fn main() {}\n"
    )));
}

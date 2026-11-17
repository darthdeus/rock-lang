//! The incremental stdlib base matches a combined pass, while the one-shot
//! semantic pass leaves the cached parse trees unchanged.

use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::{semantic_pass_into, semantic_pass_without_closure_gate};
use skull_parser::stdlib::ModuleSet;

fn user_module(user: &str) -> skull_parser::Ast {
    skull_parser::parse_module(user, Ustr::from("<test>")).expect("user parses")
}

fn with_stdlib(user: &str) -> ModuleSet {
    ModuleSet::with_stdlib([user_module(user)]).expect("stdlib parses")
}

/// What lowering and inference recorded, rendered per module. Session facts
/// (scope probes, body node intervals and path segments) are left out: a
/// one-shot pass skips them, and the shared base records them for sessions.
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
                sorted(m.bodies.iter().map(|(k, v)| {
                    let mut body = v.clone();
                    body.node_intervals.clear();
                    body.path_segments.clear();
                    format!("{k:?}={body:?}")
                })),
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

/// Check `user` through the ModuleSet and combined-pass entry points.
fn assert_same_as_combined(user: &str) {
    let mut combined = with_stdlib(user).into_vec();
    let expected = semantic_pass_into(Workspace::default(), &mut combined, &Default::default())
        .map(|ws| render(&ws))
        .map_err(|e| e.message);
    let mut modules = with_stdlib(user);
    let pass = semantic_pass_without_closure_gate(&mut modules, &Default::default());
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
}

#[test]
fn module_set_pass_matches_a_combined_pass() {
    let programs = [
        "fn main() -> int { 1 + 2 }\n",
        "fn main() { let v = vec2(1.0, 2.0); let w = v + v; print(w.x) }\n",
        "fn main() { let xs = [1, 2, 3]; for x in xs { print(x) } }\n",
        "fn main() -> int { \"not an int\" }\n",
        "struct P { x: int }\nfn main() -> int { let p = P { x: 1 }; p.y }\n",
        "fn apply<T>(x: T, f: fn(T) -> T) -> T { f(x) }\nfn main() -> int { apply(1, |x| x + 1) }\n",
    ];
    for program in programs {
        assert_same_as_combined(program);
    }
}

#[test]
fn the_retained_stdlib_base_matches_a_combined_pass() {
    let stdlib = skull_parser::stdlib::cached()
        .as_ref()
        .expect("stdlib parses");
    let retained =
        bone_semantic::stdlib_base::checked_alone(stdlib).expect("the process stdlib checks alone");
    let mut batch_modules = stdlib.to_vec();
    let batch = semantic_pass_into(
        Workspace::default(),
        &mut batch_modules,
        &Default::default(),
    )
    .expect("combined stdlib pass");
    assert_eq!(render(&retained), render(&batch));
}

#[test]
fn a_one_shot_pass_leaves_the_shared_stdlib_unexpanded() {
    let stdlib = skull_parser::stdlib::cached()
        .as_ref()
        .expect("stdlib parses");
    let before = stdlib
        .iter()
        .map(|ast| ast.items.clone())
        .collect::<Vec<_>>();
    let mut modules =
        with_stdlib("macro key(x: int) -> int { x + x }\nfn main() -> int { key(2) }\n");
    semantic_pass_without_closure_gate(&mut modules, &Default::default()).unwrap();
    assert!(modules.shared().is_empty());
    let after = stdlib
        .iter()
        .map(|ast| ast.items.clone())
        .collect::<Vec<_>>();
    assert_eq!(before, after);
}

#[test]
fn a_one_shot_pass_records_no_session_facts() {
    // Scope probes exist for the incremental session's re-lowering and body
    // node intervals for editor queries; a pass whose workspace no session
    // retains must not spend time recording them.
    let program = "fn helper(x: int) -> int { x }\nfn main() -> int { helper(1) }\n";
    let session_facts = |ws: &Workspace| {
        ws.modules
            .iter()
            .map(|module| {
                module.scope_probes.len()
                    + module
                        .bodies
                        .values()
                        .map(|body| body.node_intervals.len() + body.path_segments.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
    };
    let mut combined = with_stdlib(program).into_vec();
    let retained = semantic_pass_into(Workspace::default(), &mut combined, &Default::default());
    assert!(session_facts(&retained.unwrap()) > 0);
    // Both a shared and an already-owned input.
    for owned in [false, true] {
        let mut modules = with_stdlib(program);
        if owned {
            modules.make_owned();
        }
        let ws = semantic_pass_without_closure_gate(&mut modules, &Default::default()).unwrap();
        assert_eq!(session_facts(&ws), 0);
    }
}

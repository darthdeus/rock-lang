//! An `Iterator`-bounded body emits STANDALONE at each instantiation
//! (`docs/monomorphization-plan.md` S 12, mono cut C+D step D-1).
//!
//! A bounded fn's body holds placeholder `Inst::CallBoundMulti`s whose concrete
//! `next` is known per instantiation. Those are resolved from a
//! `ResolvedBoundTree`, and the tree reaches the body two ways: from the SITE
//! when the inliner splices it, and from the specialization DEMAND when it is
//! emitted on its own. One rewrite serves both
//! (`skull_transform::bound::substitute_bound_call`).
//!
//! With inlining DISABLED there is no splice, so this is the arm that proves the
//! demand path carries it -- and that is the plan's standing gate for E1, which
//! runs with inlining off by construction.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use ustr::Ustr;

use crate::common::AblationGuard as ModeGuard;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// A user fn taking an `Iterator`-bounded generic, called at one concrete
/// chain. `total`'s body holds the placeholder; `main` is the concrete root.
const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

/// The same program with the bounded generic REMOVED -- `total` takes the
/// concrete list. The admission control: it must emit with inlining disabled,
/// so a rejection above is about the BOUND and not about the mode refusing to
/// emit anything at all.
const CONCRETE: &str = r#"
fn total(xs: [int]) -> int {
    let s = 0
    for v in xs { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs)}")
}
"#;

/// A bounded body whose resolved witness is ITSELF bounded. `collect`'s
/// placeholder resolves to `MapIter.next`, which holds a placeholder of its own
/// for the inner `VecIter`. Only the second level needs the sub-tree that a
/// resolved leaf carries, which is why one level of nesting cannot see this.
///
/// The terminal is what forces it: a `for` over the same chain drives `next`
/// from the concrete loop header, where the witness is in hand. `.collect()`
/// hands the chain to a bounded body, so the resolution has to travel.
const NESTED: &str = r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [
        Item { id: 1, p: 3.0 },
        Item { id: 2, p: 2.0 },
    ]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#;

fn emit(src: &str) -> Result<String, String> {
    let ws = check(src);
    let mid = skull::user_module_id(&ws).expect("user module");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua).map_err(|e| e.message)
}

fn emit_err(src: &str) -> Option<String> {
    emit(src).err()
}

/// The lines of the top-level Lua function whose header line is `header`.
fn body_of<'a>(lua: &'a str, header: &str) -> Vec<&'a str> {
    lua.lines()
        .skip_while(|l| *l != header)
        .skip(1)
        .take_while(|l| !l.starts_with("function ") && *l != "end")
        .collect()
}

/// Three separate things, because each alone is satisfied by a broken build.
///
/// That emission SUCCEEDS says nothing on its own -- a build that dropped
/// `total` entirely also reports no error. That a DEFINITION exists is checked
/// on the definition line, which the caller's own call cannot satisfy. And that
/// the definition RESOLVED its placeholder is checked by the concrete `next` in
/// its body: the whole content of D-1 is that substitution, and a definition
/// emitted with the placeholder still in it would pass the first two.
#[test]
fn a_bounded_body_emits_standalone_with_inlining_disabled() {
    let _g = ModeGuard::on();
    let lua = emit(BOUNDED).expect("a bounded body must emit standalone under its demand");
    let def = lua
        .lines()
        .find(|l| l.starts_with("function total_") && l.contains("_spec_"))
        .unwrap_or_else(|| panic!("no standalone specialized definition of `total` in:\n{lua}"));
    let symbol = &def["function ".len()..def.find('(').expect("a definition header has params")];
    let body = body_of(&lua, def);
    assert!(
        body.iter().any(|l| l.contains("next_")),
        "the standalone `total` calls no concrete `next`, so its placeholder bound-call \
         was not resolved: {body:?}"
    );
    let called = lua
        .lines()
        .any(|l| !l.starts_with("function ") && l.contains(&format!("{symbol}(")));
    assert!(called, "`{symbol}` is emitted but nothing calls it");
}

/// A resolved witness that is itself bounded resolves too, one level down.
///
/// The demand a substitution files for its witness must carry the witness's OWN
/// tree -- the `sub` of the leaf that resolved it. Dropping it files the demand
/// with an EMPTY tree, and `MapIter.next` then reaches its standalone emission
/// with nothing to resolve generic #0 from. The splice path cannot see this: it
/// recurses into the callee in place, so the sub-tree never has to travel.
///
/// Asserting the emitted `MapIter.next` calls a concrete `next` is the arm that
/// distinguishes a real resolution from an emission that merely succeeded --
/// the same reason the single-level test checks the callee and not just the
/// absence of an error.
#[test]
fn a_witness_that_is_itself_bounded_resolves_from_the_leafs_sub_tree() {
    let _g = ModeGuard::on();
    let lua = emit(NESTED).expect("a bounded witness of a bounded body must emit standalone");
    let def = lua
        .lines()
        .find(|l| l.starts_with("function next_MapIter_") && l.contains("_spec_"))
        .unwrap_or_else(|| panic!("no standalone `MapIter.next` specialization in:\n{lua}"));
    let body = body_of(&lua, def);
    assert!(
        body.iter().any(|l| l.contains("next_VecIter")),
        "`MapIter.next` emitted without resolving its own placeholder to the inner \
         `VecIter.next`: {body:?}"
    );
}

/// The admission control for the sentinel. Not inverted: this asserts the
/// CORRECT behavior and must stay green through D-1.
///
/// Without it the sentinel could be green because the mode rejects every
/// program, which would say nothing about bounded bodies.
#[test]
fn an_unbounded_body_emits_with_inlining_disabled() {
    let _g = ModeGuard::on();
    let err = emit_err(CONCRETE);
    assert!(
        err.is_none(),
        "the same program without a bounded generic must still emit with \
         inlining disabled, or the sentinel cannot attribute its rejection to \
         the bound; got {err:?}"
    );
}

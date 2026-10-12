//! P2 finding fix: dense per-frame slot
//! naming. A co fn's frame cells render as `s.c<slot>` where `<slot>` is a dense
//! ordinal (0, 1, 2, ...) over the frame cells in raw-id order -- NOT the raw
//! cell-arena id. Lambda-cell allocation bumps raw ids but never reorders the
//! frame cells, so both the emitted field names AND the layout stamp keyed on
//! the slots are invariant under a lambda-internal cell-count edit. The step
//! body's frame reads and the ctor's stamp must move together (a stable stamp
//! over shifted field names would silently corrupt a live coroutine), so these
//! tests pin BOTH: the `__layout` literal AND the emitted `s.c<slot>` shape.
use crate::common::{emit_module, mangled_name};

/// The `__layout = <int>` literal the ctor `function <ctor>(` writes.
fn layout_stamp(lua: &str, ctor: &str) -> i64 {
    let ctor = mangled_name(lua, ctor);
    let marker = format!("function {ctor}(");
    let start = lua.find(&marker).unwrap_or_else(|| panic!("no ctor `{ctor}` in:\n{lua}"));
    let tail = &lua[start..];
    let key = "__layout = ";
    let lpos = tail.find(key).expect("ctor has no __layout stamp");
    let after = &tail[lpos + key.len()..];
    let end = after.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(after.len());
    after[..end].parse().expect("parse __layout stamp")
}

/// The distinct frame slots read/written in `<ctor>__step`: the step's first
/// param is the state table `s`, and a frame cell renders `<s>.c<slot>`. Returns
/// the sorted unique slots so a test can pin density (a dense 0..n).
fn frame_slots_in_step(lua: &str, ctor: &str) -> Vec<u32> {
    let ctor = mangled_name(lua, ctor);
    let head = format!("function {ctor}__step(");
    let start = lua.find(&head).unwrap_or_else(|| panic!("no `{head}` in:\n{lua}"));
    let after = &lua[start + head.len()..];
    // First param name (the state table), up to `,` or `)`.
    let pend = after.find([',', ')']).expect("step has a param list");
    let sname = after[..pend].trim().to_string();
    // Step body ends at the first column-0 `\nend\n`.
    let body_end = after.find("\nend\n").map(|e| e + 5).unwrap_or(after.len());
    let body = &after[..body_end];

    let needle = format!("{sname}.c");
    let mut slots: Vec<u32> = Vec::new();
    let mut rest = body;
    while let Some(p) = rest.find(&needle) {
        let digits = &rest[p + needle.len()..];
        let n = digits.bytes().take_while(|b| b.is_ascii_digit()).count();
        if n > 0 {
            if let Ok(slot) = digits[..n].parse::<u32>() {
                if !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
        }
        rest = &rest[p + needle.len()..];
    }
    slots.sort_unstable();
    slots
}

/// A co fn whose two frame cells (`base`, the record `f`) render as `s.c0`/`s.c1`
/// -- dense from 0, not the raw cell-arena ids.
const DENSE: &str = r#"
enum Ev { Tick(int) }
co fn gen() -> Coroutine<Ev> {
    let base = 4
    let f = (x: int) => x + base
    yield Ev::Tick(f(3))
    yield Ev::Tick(f(3))
}
fn main() { let c = gen(); let _ = c.resume() }
"#;

#[test]
fn frame_fields_are_dense_from_zero() {
    let lua = emit_module(DENSE);
    let slots = frame_slots_in_step(&lua, "gen");
    assert!(
        slots.len() >= 2 && slots == (0..slots.len() as u32).collect::<Vec<_>>(),
        "frame slots must be dense c0..c<n-1>, got {slots:?}:\n{lua}"
    );
}

/// Two sources differing ONLY by a used lambda-internal `let` produce the SAME
/// `__layout` stamp AND the same set of frame-field slots -- the used local
/// homes to the lambda's hoisted body (a plain local), never the frame, and the
/// raw-id shift it causes is absorbed by the dense slots.
#[test]
fn lambda_internal_let_is_layout_stable() {
    let prog = |body: &str| {
        format!(
            "enum Ev {{ Tick(int) }}\n\
             co fn gen() -> Coroutine<Ev> {{\n  let base = 4\n  let f = (x: int) => {body}\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n}}\n\
             fn main() {{ let c = gen(); let _ = c.resume() }}\n"
        )
    };
    let a = emit_module(&prog("x + base"));
    let b = emit_module(&prog("{ let y = base + base; x + y }"));
    assert_eq!(
        layout_stamp(&a, "gen"),
        layout_stamp(&b, "gen"),
        "a used lambda-internal let must not shift the layout stamp"
    );
    assert_eq!(
        frame_slots_in_step(&a, "gen"),
        frame_slots_in_step(&b, "gen"),
        "the frame-field slots must be identical (names and stamp move together)"
    );
}

/// The converse guard: a co-fn-LEVEL `let` adds a real frame cell, so it MUST
/// shift the stamp -- proving the slot map didn't over-stabilize (a stable stamp
/// across a genuine layout change would be the corruption hazard).
#[test]
fn cofn_level_let_shifts_layout() {
    let prog = |extra: &str| {
        format!(
            "enum Ev {{ Tick(int) }}\n\
             fn mk(n: int) -> int {{ return n }}\n\
             co fn gen() -> Coroutine<Ev> {{\n  let base = mk(4)\n{extra}  let f = (x: int) => x + base\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n}}\n\
             fn main() {{ let c = gen(); let _ = c.resume() }}\n"
        )
    };
    let a = emit_module(&prog(""));
    let b = emit_module(&prog("  let z = mk(9)\n  yield Ev::Tick(z)\n"));
    assert_ne!(
        layout_stamp(&a, "gen"),
        layout_stamp(&b, "gen"),
        "a co-fn-level local adds a frame cell and must shift the stamp"
    );
}

/// An UNUSED lambda-internal `let y = 7` is a ghost: dead, referenced in no
/// block, so it must NOT become a frame field nor change the `__layout` stamp
/// (cause B of the finding). Compared to the same co fn without the dead local.
#[test]
fn ghost_lambda_local_is_not_a_frame_field() {
    let base = r#"
enum Ev { Tick(int) }
co fn gen() -> Coroutine<Ev> {
    let base = 4
    let f = (x: int) => x + base
    yield Ev::Tick(f(3))
    yield Ev::Tick(f(3))
}
fn main() { let c = gen(); let _ = c.resume() }
"#;
    let ghost = r#"
enum Ev { Tick(int) }
co fn gen() -> Coroutine<Ev> {
    let base = 4
    let f = (x: int) => { let y = 7; x + base }
    yield Ev::Tick(f(3))
    yield Ev::Tick(f(3))
}
fn main() { let c = gen(); let _ = c.resume() }
"#;
    let a = emit_module(base);
    let b = emit_module(ghost);
    assert_eq!(
        layout_stamp(&a, "gen"),
        layout_stamp(&b, "gen"),
        "a dead lambda-local (ghost) must not enter the frame / change the stamp"
    );
    assert_eq!(
        frame_slots_in_step(&a, "gen"),
        frame_slots_in_step(&b, "gen"),
        "the ghost must not add a frame field"
    );
    // And the dead local's value literal (7) must not appear as a frame store.
    let step_head = format!("function {}__step(", mangled_name(&b, "gen"));
    let s = &b[b.find(&step_head).unwrap()..];
    let step = &s[..s.find("\nend\n").map(|e| e + 5).unwrap_or(s.len())];
    assert!(!step.contains("= (7)"), "the ghost's dead store must be DCE'd:\n{step}");
}

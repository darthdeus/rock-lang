//! Pushing a `value` struct into a `[T]` from a context the inliner never
//! splices (a closure body, a nested fn) must store the WHOLE struct, not a
//! scalarized leaf.
//!
//! The call site lowers `xs.push(v(..))` with the SPECIALIZED scalar ABI (the
//! Vec2 arg splats to `(x, y)`, expecting the inliner to splice a specialized
//! `push` whose body takes leaf params). But the inliner refuses closure
//! sub-CFGs (upvalue cap) and never runs on hoisted nested fns, so the call
//! survives pointing at the SHARED generic `push(vec, item)` body -- which binds
//! `item = x` and drops `y`, storing a number. Reading `xs[0].x` then indexes a
//! number (runtime crash). The fix: don't take the specialized-splat ABI in a
//! context that won't be spliced -- box the value arg to match the shared body.
use crate::common::emit_module;

fn run_prog(lua: &str, tag: &str) -> String {
    let path = std::env::temp_dir().join(format!("skull_{tag}_{}.lua", std::process::id()));
    std::fs::write(&path, format!("{lua}\nmain()\n")).expect("write");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "luajit failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const CLOSURE_PUSH: &str = r#"
fn run(f: () -> ()) { f() }
fn main() {
    let xs: [Vec2] = []
    run(() => { xs.push(v(7.0, 9.0)) })
    print("{xs[0].x} {xs[0].y}")
}
"#;

const NESTED_FN_PUSH: &str = r#"
fn main() {
    let xs: [Vec2] = []
    fn fill(ys: [Vec2]) { ys.push(v(3.0, 4.0)) }
    fill(xs)
    print("{xs[0].x} {xs[0].y}")
}
"#;

/// Baseline: the SAME push directly in main (no closure) already works -- the
/// call inlines. Pins that the fix doesn't regress the inlined path.
const DIRECT_PUSH: &str = r#"
fn main() {
    let xs: [Vec2] = []
    xs.push(v(7.0, 9.0))
    print("{xs[0].x} {xs[0].y}")
}
"#;

/// A value arg at a NON-first param position (`Vec.set(vec, i, value: T)`, an
/// `@inline` generic) through the same closure boundary -- the other end of the
/// same scalarization-decision site.
const CLOSURE_SET: &str = r#"
fn run(f: () -> ()) { f() }
fn main() {
    let xs: [Vec2] = [v(0.0, 0.0)]
    run(() => { xs.set(0, v(33.0, 44.0)) })
    print("{xs[0].x} {xs[0].y}")
}
"#;

#[test]
fn value_push_in_closure_stores_whole_struct() {
    let lua = emit_module(CLOSURE_PUSH);
    assert_eq!(run_prog(&lua, "closure_push"), "7 9");
}

#[test]
fn value_push_in_nested_fn_stores_whole_struct() {
    let lua = emit_module(NESTED_FN_PUSH);
    assert_eq!(run_prog(&lua, "nested_push"), "3 4");
}

#[test]
fn value_push_direct_stores_whole_struct() {
    let lua = emit_module(DIRECT_PUSH);
    assert_eq!(run_prog(&lua, "direct_push"), "7 9");
}

#[test]
fn value_set_in_closure_stores_whole_struct() {
    let lua = emit_module(CLOSURE_SET);
    assert_eq!(run_prog(&lua, "closure_set"), "33 44");
}

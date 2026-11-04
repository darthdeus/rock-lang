//! A bounded fn exists ONLY monomorphized-by-inlining, so a lambda literal in
//! its body must not make it un-inlinable: `fn fwd<I: Iterator<int>>(it: I) {
//! it.reduce(0, (a, x) => a + x) }` forwards its generic receiver into a
//! fn-value terminal with the closure constructed in its own body. The caller
//! must fully inline the chain; a refusal leaves a residual bounded call and the caller cannot emit.
use crate::common::{emit_mir, run_luajit};

const FWD_TERMINALS: &str = r#"
fn fwd_reduce<I: Iterator<int>>(it: I) -> int {
    return it.reduce(0, (acc, x) => acc + x)
}

fn fwd_any<I: Iterator<int>>(it: I) -> bool {
    return it.any(x => x > 2)
}

fn fwd_find<I: Iterator<int>>(it: I) -> int {
    let r = it.find(x => x > 1)
    return r ?? -1
}

fn fwd_chained<I: Iterator<int>>(it: I) -> int {
    return it.skip(1).reduce(0, (acc, x) => acc + x)
}

fn main() {
    print(fwd_reduce([1,2,3].iter()))
    print(fwd_any([1,2,3].iter()))
    print(fwd_find([1,2,3].iter()))
    print(fwd_chained([1,2,3].iter()))
}
"#;

#[test]
fn bounded_fn_with_lambda_literal_inlines_and_runs() {
    let emit = emit_mir(FWD_TERMINALS);
    let bad: Vec<&(String, String)> = emit
        .skipped
        .iter()
        .filter(|(name, _)| name == "main" || name.starts_with("fwd_"))
        .collect();
    assert!(
        bad.is_empty(),
        "caller/forwarder dropped from the emit: {bad:?}"
    );
    let out = run_luajit(&format!("{}\nmain()\n", emit.lua), "bounded_lambda");
    assert_eq!(out, "6\ntrue\n2\n5");
}

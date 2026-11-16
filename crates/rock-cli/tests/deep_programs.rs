//! Function bodies a target refuses to load (LuaJIT's jump range, V8's parser
//! depth) run when the emitter can flatten them and otherwise fail at compile
//! time with a located error, never at the point the emitted chunk first runs.
use crate::support::rock_test_source;

fn else_if_chain(arms: usize) -> String {
    let mut src = "fn f(x: int) -> int {\n    let r = 0\n    if x == 0 { r = 0 }\n".to_string();
    for i in 1..arms {
        src.push_str(&format!("    else if x == {i} {{ r = {i} }}\n"));
    }
    src.push_str("    return r\n}\n@test\nfn t() { assert(f(3) == 3) }\n");
    src
}

#[test]
fn a_chain_printed_as_one_elseif_ladder_runs() {
    // Structured output spends about three instructions per rung, where label
    // and goto output spent six and ran out of jump range near 5500 rungs.
    let (ok, text) = rock_test_source(&else_if_chain(6000), &[]);
    assert!(ok, "{text}");
}

#[test]
fn a_chain_past_the_jump_range_is_a_located_error() {
    // The first rung's escape jump to the ladder's `end` spans every rung.
    let (ok, text) = rock_test_source(&else_if_chain(20000), &[]);
    assert!(!ok, "{text}");
    assert!(text.contains("too large for LuaJIT"), "{text}");
    assert!(text.contains("control structure too long"), "{text}");
    assert!(text.contains("probe.rock:"), "no location: {text}");
    assert!(!text.contains("execution failed"), "{text}");
}

#[cfg(feature = "backend-js")]
mod js {
    use super::else_if_chain;
    use crate::support::{require_node, rock_test_source};

    const JS: &[&str] = &["--backend", "js"];

    #[test]
    fn a_long_operator_chain_runs() {
        if !require_node() {
            return;
        }
        let sum = vec!["1"; 2000].join(" + ");
        let src =
            format!("fn f() -> int {{ return {sum} }}\n@test\nfn t() {{ assert(f() == 2000) }}\n");
        let (ok, text) = rock_test_source(&src, JS);
        assert!(ok, "{text}");
    }

    #[test]
    fn a_long_else_if_chain_runs() {
        if !require_node() {
            return;
        }
        let (ok, text) = rock_test_source(&else_if_chain(2000), JS);
        assert!(ok, "{text}");
    }

    #[test]
    fn nesting_past_the_parser_limit_is_a_located_error() {
        let n = 2000;
        let mut src = "fn f(x: int) -> int {\n".to_string();
        src.push_str(&"    if x == 0 {\n".repeat(n));
        src.push_str("    return 1\n");
        src.push_str(&"    }\n".repeat(n));
        src.push_str("    return 0\n}\n@test\nfn t() { assert(f(0) == 1) }\n");
        let (ok, text) = rock_test_source(&src, JS);
        assert!(!ok, "{text}");
        assert!(text.contains("nests control flow too deeply"), "{text}");
        assert!(text.contains("probe.rock:"), "no location: {text}");
        assert!(!text.contains("js backend bug"), "{text}");
    }
}

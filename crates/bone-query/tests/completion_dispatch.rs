//! Dispatcher fallbacks that must preserve member and general completion.

use crate::completion::*;

#[test]
fn member_completion_offers_the_enclosing_ufcs_function() {
    for expression in ["v.<|>", "unknown(v.<|>)"] {
        let src = format!("struct V {{ x: int }}\nfn r(v: V) {{\n  {expression}\n}}\n");
        let items = complete(&src);
        assert_eq!(labels_of(&items), ["r", "x: int"], "{expression}");
        assert_insert(expression, find(&items, "r"), Insert::Plain("r()"));
        assert_insert(expression, find(&items, "x: int"), Insert::Text("x"));
    }
}

#[test]
fn a_scalar_call_receiver_falls_through_to_general_completion() {
    let items = complete(
        "fn scale(x: int) -> int { return x }\nfn sc_two() {}\n\
         fn r(x: int) {\n  let n = scale(x).sc<|>\n}\n",
    );
    assert_ranked("scalar receiver", &items, "scale(x) < scale < sc_two");
    assert_insert(
        "call through",
        find(&items, "scale(x)"),
        Insert::Text("scale(x)"),
    );
    assert_insert(
        "function",
        find(&items, "scale"),
        Insert::Snippet("scale($0)"),
    );
    assert_insert(
        "nullary function",
        find(&items, "sc_two"),
        Insert::Plain("sc_two()"),
    );
}

#[test]
fn single_file_fallback_member_completion_reaches_only_its_own_modules() {
    let items = complete(
        "value Vary { id: float }\nfn frag(vary: Vary, t: Missing) -> int {\n  return vary.i<|> as int\n}\n",
    );
    assert_offers(&labels_of(&items), &["id: float"]);
}

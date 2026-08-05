//! `bone_core::hir::OPERATOR_FNS` classifies every operator symbol as
//! overloadable or not, and `pass_lower_functions` rejects a user overload of
//! the second group. That table is a restatement of the parser's symbol match
//! (`skull-parser/src/parser.rs`, `parse_operator`), so it can drift -- and a
//! symbol missing from it would be misreported as a reserved name rather than
//! classified. These tests tie the two together in both directions.
//!
//! The behavioural halves live elsewhere, one per side of the admission:
//! `tests/errors/non_overloadable_operator_def.rock.err` asserts each
//! non-overloadable symbol REJECTS, and `tests/overloadable_operators.rock`
//! asserts each overloadable one actually DISPATCHES to the user fn.

use bone_core::hir::{operator_entry, OPERATOR_FNS};
use skull_parser::ItemKind;
use ustr::Ustr;

/// Parse `operator <symbol>(..)` and report the fn name the parser minted.
fn parser_fn_name_for(symbol: &str) -> Option<String> {
    // Unary `!` and `~` take one param; `[]` is spelled with the index in
    // parens. Arity does not affect the name, so one shape per group is enough.
    let src = match symbol {
        "!" | "~" => format!("value V {{ a: int }}\noperator {symbol}(x: V) -> V {{ return x }}\n"),
        "[]" => "value V { a: int }\noperator [](x: V, i: int) -> int { return 1 }\n".to_string(),
        _ => format!(
            "value V {{ a: int }}\noperator {symbol}(x: V, y: V) -> V {{ return x }}\n"
        ),
    };
    let ast = skull_parser::parse_module_clean(&src, Ustr::from("<operator_table>")).ok()?;
    ast.items.iter().find_map(|item| match &item.kind {
        ItemKind::Function(f) if f.name.name.as_str().starts_with("__op_") => {
            Some(f.name.name.to_string())
        }
        _ => None,
    })
}

/// Every symbol in the table must still parse to the fn name the table claims.
/// A renamed or dropped parser arm shows up here rather than as a mystery
/// "reserved operator name" rejection at a user's call site.
#[test]
fn operator_table_matches_the_parser() {
    let mut wrong = Vec::new();
    for (fn_name, symbol, _) in OPERATOR_FNS {
        match parser_fn_name_for(symbol) {
            Some(parsed) if parsed == *fn_name => {}
            other => wrong.push(format!("`{symbol}` -> {other:?}, table says `{fn_name}`")),
        }
    }
    assert!(
        wrong.is_empty(),
        "OPERATOR_FNS disagrees with the parser for {} of {} symbols:\n  {}",
        wrong.len(),
        OPERATOR_FNS.len(),
        wrong.join("\n  ")
    );
}

/// The classification is a real partition, not an accident of one side being
/// empty -- a table that marked everything overloadable would make the
/// rejection dead while every other test still passed.
#[test]
fn operator_table_has_both_classes_populated() {
    let overloadable = OPERATOR_FNS.iter().filter(|(_, _, ok)| *ok).count();
    let rejected = OPERATOR_FNS.len() - overloadable;
    assert_eq!(
        overloadable, 6,
        "expected exactly `+ - * / % ./` to be overloadable, got {overloadable}"
    );
    assert!(
        rejected >= 15,
        "the non-overloadable class collapsed to {rejected} entries"
    );
}

/// `operator_entry` must answer for operator names and decline for others, so
/// the rejection's `None` arm stays reachable only for a genuinely unknown name.
#[test]
fn operator_entry_declines_non_operator_names() {
    assert_eq!(operator_entry("__op_add"), Some(("+", true)));
    assert_eq!(operator_entry("__op_eq"), Some(("==", false)));
    assert_eq!(operator_entry("main"), None);
    assert_eq!(operator_entry("__op_nonexistent"), None);
}

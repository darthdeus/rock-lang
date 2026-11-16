//! Position-blind invariants that hold for every completion response,
//! whatever the cursor context. `support/completion.rs` checks them on every
//! query, so one invariant guards a bug class across all fixtures.

use lsp_types::CompletionItem;

/// Violations of the invariants; empty = clean. `typed_prefix` is the
/// identifier prefix at the cursor, in the case the user typed.
pub fn completion_response_violations(items: &[CompletionItem], typed_prefix: &str) -> Vec<String> {
    let mut violations = Vec::new();

    // Two items with one label are indistinguishable in the popup.
    let mut seen = std::collections::HashSet::new();
    for it in items {
        if !seen.insert(it.label.as_str()) {
            violations.push(format!("duplicate label `{}`", it.label));
        }
    }

    // A foreign sort_text means some path bypassed the RankKey encoder.
    for it in items {
        match it.sort_text.as_deref() {
            None => violations.push(format!("`{}` has no sort_text", it.label)),
            Some(st) => {
                if bone_query::explain_sort_text(st).is_none() {
                    violations.push(format!(
                        "`{}` sort_text does not decode as a RankKey: {st:?}",
                        it.label
                    ));
                }
            }
        }
    }

    // A client may sort by sortText or take the array as-is; both must show
    // the server's ranking.
    for pair in items.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if let (Some(sa), Some(sb)) = (a.sort_text.as_deref(), b.sort_text.as_deref()) {
            if sa > sb {
                violations.push(format!(
                    "array order disagrees with sortText order: `{}` ({sa:?}) \
                     precedes `{}` ({sb:?})",
                    a.label, b.label
                ));
            }
        }
    }

    // An item labelled exactly the typed prefix may only be outranked by
    // another exact-tier item (such as a named arg `color:` whose rank name
    // is the prefix). `exact` leads the RankKey, so anything else above it
    // skipped the exact tier.
    if !typed_prefix.is_empty() {
        if let Some(exact_st) = items
            .iter()
            .find(|it| it.label == typed_prefix)
            .and_then(|it| it.sort_text.as_deref())
        {
            for it in items {
                let Some(st) = it.sort_text.as_deref() else {
                    continue;
                };
                if st < exact_st && !st.starts_with('0') {
                    violations.push(format!(
                        "non-exact `{}` ranks above the exact match `{typed_prefix}` \
                         ({st:?} < {exact_st:?})",
                        it.label
                    ));
                }
            }
        }
    }

    violations
}

mod tests {
    use super::*;

    fn item(label: &str, sort_text: Option<&str>) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            sort_text: sort_text.map(str::to_string),
            ..Default::default()
        }
    }

    /// A sort_text in the RankKey layout: `exact`, `type_rank` and `band`
    /// lead, the other fifteen digits are zero, and the name breaks ties.
    fn key(exact: u8, type_rank: u8, band: u8, name: &str) -> String {
        format!("{exact}{type_rank}{band}{:015}{name}", 0)
    }

    fn st(name: &str) -> String {
        key(0, 0, 0, name)
    }

    #[test]
    fn clean_response_has_no_violations() {
        let items = [
            item("alpha", Some(&st("alpha"))),
            item("beta", Some(&st("beta"))),
        ];
        assert!(completion_response_violations(&items, "al").is_empty());
    }

    #[test]
    fn duplicate_labels_reported() {
        let items = [item("x", Some(&st("x"))), item("x", Some(&st("x")))];
        let v = completion_response_violations(&items, "");
        assert!(v.iter().any(|m| m.contains("duplicate label `x`")), "{v:?}");
    }

    #[test]
    fn missing_and_undecodable_sort_texts_reported() {
        let items = [item("a", None), item("b", Some("not-a-rank-key"))];
        let v = completion_response_violations(&items, "");
        assert!(
            v.iter().any(|m| m.contains("`a` has no sort_text")),
            "{v:?}"
        );
        assert!(
            v.iter()
                .any(|m| m.contains("`b` sort_text does not decode")),
            "{v:?}"
        );
    }

    #[test]
    fn array_order_disagreeing_with_sort_text_reported() {
        let first = key(0, 0, 2, "zeta");
        let second = st("alpha");
        // `zeta` (worse key) placed before `alpha` (better key) in the array.
        let items = [item("zeta", Some(&first)), item("alpha", Some(&second))];
        let v = completion_response_violations(&items, "");
        assert!(
            v.iter().any(|m| m.contains("array order disagrees")),
            "{v:?}"
        );
        // Sorted arrays are clean.
        let ok = [item("alpha", Some(&second)), item("zeta", Some(&first))];
        assert!(completion_response_violations(&ok, "").is_empty());
    }

    #[test]
    fn non_exact_ranking_above_exact_reported() {
        // The failure mode: a path fails to put the exact-label item in the
        // exact tier (its own key leads with 1), so a non-exact item can
        // sort below it -- the invariant flags the non-exact item above.
        let above = key(1, 0, 0, "weapon");
        let missed_exact = key(1, 2, 0, "wea");
        let items = [
            item("weapon", Some(&above)),
            item("wea", Some(&missed_exact)),
        ];
        let v = completion_response_violations(&items, "wea");
        assert!(
            v.iter()
                .any(|m| m.contains("non-exact `weapon` ranks above")),
            "{v:?}"
        );
    }

    #[test]
    fn exact_tier_item_above_exact_match_is_clean() {
        // A named-arg style item (`wea:`) whose RANK name is exactly the
        // prefix legitimately outranks the exact-label item on type_rank --
        // both are exact-tier.
        let named = st("wea");
        let local = key(0, 2, 0, "wea");
        let items = [item("wea:", Some(&named)), item("wea", Some(&local))];
        assert!(completion_response_violations(&items, "wea").is_empty());
    }
}

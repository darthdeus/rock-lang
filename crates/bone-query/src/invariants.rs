//! Position-blind completion-response invariants (completion-v2 M4): the
//! properties that hold for EVERY completion response regardless of cursor
//! context, checked over the ranking corpus (test helpers) AND real-project
//! positions (`rock completion-sweep --check`). One invariant catches a bug
//! class across all contexts where a per-case guard pins a single decision.
//!
//! Context-DEPENDENT invariants (typed-slot ordering, universe gating) live
//! in `tests/completion_invariants.rs` with fixtures that know their slot.
//! The array-order == sortText-order invariant is deliberately absent until
//! the diversify question resolves (docs/unfuck-backlog.md #30: `diversify`
//! reorders the array but sortText still encodes the pre-diversity rank).

use lsp_types::CompletionItem;

/// Violations of the position-blind response invariants; empty = clean.
/// `typed_prefix` is the identifier prefix at the cursor (RAW case, as the
/// user typed it).
pub fn completion_response_violations(
    items: &[CompletionItem],
    typed_prefix: &str,
) -> Vec<String> {
    let mut violations = Vec::new();

    // No duplicate labels: two items with one label are indistinguishable in
    // the popup -- always a dedup bug (sink `admit` / query-level `seen`).
    let mut seen = std::collections::HashSet::new();
    for it in items {
        if !seen.insert(it.label.as_str()) {
            violations.push(format!("duplicate label `{}`", it.label));
        }
    }

    // Every sort_text decodes as a RankKey encoding: a foreign sort_text
    // means some path bypassed the ONE encoder (rank_key.rs).
    for it in items {
        match it.sort_text.as_deref() {
            None => violations.push(format!("`{}` has no sort_text", it.label)),
            Some(st) => {
                if crate::rank_key::explain_sort_text(st).is_none() {
                    violations.push(format!(
                        "`{}` sort_text does not decode as a RankKey: {st:?}",
                        it.label
                    ));
                }
            }
        }
    }

    // Exact-tier dominance: an item whose label IS the typed prefix
    // (case-exact) may only be outranked by another EXACT-tier item (a
    // named-arg `color:` whose rank name is also exactly the prefix, a
    // type-preferred exact) -- `exact` is the RankKey's leading field, so a
    // non-exact item sorting above an exact one means some path skipped the
    // exact tier. Rank order is SORT_TEXT order, not array order: the array
    // is diversity-reordered (general path) or unsorted (member path); a
    // spec-following client sorts by sort_text.
    if !typed_prefix.is_empty() {
        if let Some(exact_st) =
            items.iter().find(|it| it.label == typed_prefix).and_then(|it| it.sort_text.as_deref())
        {
            for it in items {
                let Some(st) = it.sort_text.as_deref() else { continue };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, sort_text: Option<&str>) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            sort_text: sort_text.map(str::to_string),
            ..Default::default()
        }
    }

    /// A well-formed sort_text for tests: the real encoder's output.
    fn st(name: &str) -> String {
        crate::rank_key::RankKey::default().encode(name)
    }

    #[test]
    fn clean_response_has_no_violations() {
        let items =
            [item("alpha", Some(&st("alpha"))), item("beta", Some(&st("beta")))];
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
        assert!(v.iter().any(|m| m.contains("`a` has no sort_text")), "{v:?}");
        assert!(v.iter().any(|m| m.contains("`b` sort_text does not decode")), "{v:?}");
    }

    #[test]
    fn non_exact_ranking_above_exact_reported() {
        use crate::rank_key::RankKey;
        // The failure mode: a path fails to put the exact-label item in the
        // exact tier (its own key leads with 1), so a non-exact item can
        // sort below it -- the invariant flags the non-exact item above.
        let above = RankKey { exact: 1, ..Default::default() }.encode("weapon");
        let missed_exact = RankKey { exact: 1, type_rank: 2, ..Default::default() }.encode("wea");
        let items = [item("weapon", Some(&above)), item("wea", Some(&missed_exact))];
        let v = completion_response_violations(&items, "wea");
        assert!(v.iter().any(|m| m.contains("non-exact `weapon` ranks above")), "{v:?}");
    }

    #[test]
    fn exact_tier_item_above_exact_match_is_clean() {
        use crate::rank_key::RankKey;
        // A named-arg style item (`wea:`) whose RANK name is exactly the
        // prefix legitimately outranks the exact-label item on type_rank --
        // both are exact-tier. Array order is irrelevant to the invariant.
        let named = RankKey::default().encode("wea");
        let local = RankKey { type_rank: 2, ..Default::default() }.encode("wea");
        let items = [item("wea", Some(&local)), item("wea:", Some(&named))];
        assert!(completion_response_violations(&items, "wea").is_empty());
    }
}

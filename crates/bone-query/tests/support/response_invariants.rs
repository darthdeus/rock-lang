//! Position-blind invariants that hold for every completion response.

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

mod tests {}

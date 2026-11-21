use crate::support;
use bone_lints::{Finding, IncrementalLintCache, LintCacheUpdate, LintOptions, UNUSED_VAR};
use bone_semantic::prelude::*;

const FILE: &str = "<incremental-cache>";

fn run_update(
    cache: &mut IncrementalLintCache,
    ws: &Workspace,
    sources: &FxHashMap<Ustr, &str>,
    options: &LintOptions,
    change: &LintCacheUpdate,
) -> Vec<Finding> {
    cache.update(ws, &support::warm(ws), true, sources, options, change)
}

/// The function `name` wherever it is declared in `ws`.
fn function_owner(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    ws.modules
        .iter()
        .find_map(|module| {
            module
                .functions
                .iter()
                .find(|(_, function)| function.name.rsplit("::").next() == Some(name))
                .map(|(fid, _)| (module.id, BodyOwner::Function(fid)))
        })
        .expect("function")
}

/// A cache after a full lint of `parts`.
fn seeded_cache(parts: &[(&str, &str)], selection: &LintOptions) -> IncrementalLintCache {
    let mut cache = IncrementalLintCache::default();
    let (ws, sources) = support::setup(parts);
    let _ = run_update(
        &mut cache,
        &ws,
        &sources,
        selection,
        &LintCacheUpdate::full(),
    );
    cache
}

fn seed(src: &str) -> IncrementalLintCache {
    seeded_cache(&[(FILE, src)], &LintOptions::default())
}

/// Move `cache` to `parts` as a body edit of `changed`, re-inferring
/// `function`.
fn body_edit(
    cache: &mut IncrementalLintCache,
    parts: &[(&str, &str)],
    selection: &LintOptions,
    changed: &[&str],
    function: Option<&str>,
) -> Vec<Finding> {
    let (ws, sources) = support::setup(parts);
    let reinferred = function.map(|name| function_owner(&ws, name));
    let change = LintCacheUpdate::body_only(changed.iter().map(|f| Ustr::from(f)), reinferred);
    run_update(cache, &ws, &sources, selection, &change)
}

/// `body_edit` of the single file `FILE` with every default lint.
fn edit(cache: &mut IncrementalLintCache, src: &str, function: Option<&str>) -> Vec<Finding> {
    body_edit(
        cache,
        &[(FILE, src)],
        &LintOptions::default(),
        &[FILE],
        function,
    )
}

#[test]
fn suppression_edits_hide_and_restore_findings() {
    for (old, suppressed, lint) in [
        (
            "fn main() {\n    let unused = 1\n}\n",
            "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n",
            UNUSED_VAR,
        ),
        (
            "fn dead() {}\nfn main() {}\n",
            "// rock: allow(unused_fn)\nfn dead() {}\nfn main() {}\n",
            bone_lints::UNUSED_FN,
        ),
    ] {
        let mut cache = seed(old);
        let hidden = edit(&mut cache, suppressed, None);
        assert!(hidden.is_empty());
        let shown = edit(&mut cache, old, Some("main"));
        assert_eq!(shown.iter().filter(|f| f.lint == lint).count(), 1);
    }
}

#[test]
fn findings_keep_source_order_after_a_body_edit() {
    let src = "fn first(a: int) {\n    a == 1\n    let first_unused = 1\n}\nfn second(b: int) {\n    b == 2\n    let second_unused = 2\n}\nfn main() { first(1); second(2) }\n";
    let mut cache = seed(src);
    let findings = edit(&mut cache, src, Some("second"));
    assert_eq!(findings.len(), 4, "{findings:#?}");
    let starts: Vec<_> = findings
        .iter()
        .map(|finding| finding.error.labels[0].span.offset_range().0)
        .collect();
    assert!(
        starts.windows(2).all(|pair| pair[0] <= pair[1]),
        "{starts:?}"
    );
}

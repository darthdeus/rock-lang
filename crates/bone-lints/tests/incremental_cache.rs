use crate::support;
use bone_lints::{
    Finding, IncrementalLintCache, LintCacheStats, LintCacheUpdate, LintOptions, TextEditDelta,
    UNUSED_VAR, lint_workspace,
};
use bone_semantic::prelude::*;

const FILE: &str = "<incremental-cache>";
const ACTION_FILE: &str = "app/player.rock";

fn run_update(
    cache: &mut IncrementalLintCache,
    ws: &Workspace,
    sources: &FxHashMap<Ustr, &str>,
    options: &LintOptions,
    change: &LintCacheUpdate,
) -> (Vec<Finding>, LintCacheStats) {
    cache.update(ws, &support::warm(ws), true, sources, options, change)
}

fn lint_fresh(
    ws: &Workspace,
    sources: &FxHashMap<Ustr, &str>,
    options: &LintOptions,
) -> Vec<Finding> {
    lint_workspace(ws, &support::warm(ws), true, sources, options)
}

fn assert_matches_fresh(
    cached: &[Finding],
    ws: &Workspace,
    sources: &FxHashMap<Ustr, &str>,
    options: &LintOptions,
) {
    assert_eq!(
        format!("{cached:#?}"),
        format!("{:#?}", lint_fresh(ws, sources, options))
    );
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

fn action_selection() -> LintOptions {
    LintOptions::only([bone_lints::ACTION_CALLERS.to_owned()])
        .with_action_policy(Some(&support::POLICY))
}

fn sibling_selection() -> LintOptions {
    LintOptions::only([bone_lints::BACKEND_SIBLING_MISMATCH.to_owned()])
}

/// A cache after a full lint of `parts`, which must agree with a fresh lint.
fn seeded_cache(parts: &[(&str, &str)], selection: &LintOptions) -> IncrementalLintCache {
    let mut cache = IncrementalLintCache::default();
    let (ws, sources) = support::setup(parts);
    let (rebuilt, _) = run_update(
        &mut cache,
        &ws,
        &sources,
        selection,
        &LintCacheUpdate::full(),
    );
    assert_matches_fresh(&rebuilt, &ws, &sources, selection);
    cache
}

fn seed(src: &str) -> IncrementalLintCache {
    seeded_cache(&[(FILE, src)], &LintOptions::default())
}

/// Text in `FILE` from `at` on moved from `old` to `new` and by `lines` lines.
fn shift(at: usize, old: &str, new: &str, lines: isize) -> TextEditDelta {
    TextEditDelta {
        file: Ustr::from(FILE),
        at,
        byte_delta: new.len() as isize - old.len() as isize,
        line_delta: lines,
    }
}

/// Move `cache` to `parts` as a body edit of `changed`, re-inferring
/// `function`, and check it against a fresh lint.
fn body_edit(
    cache: &mut IncrementalLintCache,
    parts: &[(&str, &str)],
    selection: &LintOptions,
    changed: &[&str],
    function: Option<&str>,
    edit: Option<TextEditDelta>,
) -> (Vec<Finding>, LintCacheStats) {
    let (ws, sources) = support::setup(parts);
    let reinferred = function.map(|name| function_owner(&ws, name));
    let change =
        LintCacheUpdate::body_only(changed.iter().map(|f| Ustr::from(f)), reinferred, edit);
    let (cached, stats) = run_update(cache, &ws, &sources, selection, &change);
    assert_matches_fresh(&cached, &ws, &sources, selection);
    (cached, stats)
}

/// `body_edit` of the single file `FILE` with every default lint.
fn edit(
    cache: &mut IncrementalLintCache,
    src: &str,
    function: Option<&str>,
    edit: Option<TextEditDelta>,
) -> (Vec<Finding>, LintCacheStats) {
    body_edit(
        cache,
        &[(FILE, src)],
        &LintOptions::default(),
        &[FILE],
        function,
        edit,
    )
}

#[test]
fn local_body_findings_can_be_added_and_removed() {
    let mut cache = seed("fn main() { let used = 1\n print(used) }\n");
    let added = "fn main() { let unused = 1\n let used = 1\n print(used) }\n";
    let (findings, stats) = edit(&mut cache, added, Some("main"), None);
    assert!(findings.iter().any(|f| f.lint == UNUSED_VAR));
    assert!(stats.flat_body_requests > 0);

    let (findings, stats) = edit(&mut cache, "fn main() { print(1) }\n", Some("main"), None);
    assert!(findings.iter().all(|f| f.lint != UNUSED_VAR));
    assert!(stats.flat_body_requests > 0);
}

#[test]
fn suppression_add_and_remove_refilters_without_rederiving_findings() {
    let old = "fn main() {\n    let unused = 1\n}\n";
    let suppressed = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let at = suppressed.find("// rock").expect("suppression");
    let mut cache = seed(old);

    let (hidden, stats) = edit(
        &mut cache,
        suppressed,
        None,
        Some(shift(at, old, suppressed, 1)),
    );
    assert!(hidden.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.flat_body_requests, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.suppression_invalidations > 0);
    assert!(stats.suppression_files_refiltered > 0);

    let (shown, stats) = edit(
        &mut cache,
        old,
        Some("main"),
        Some(shift(at, suppressed, old, -1)),
    );
    assert!(shown.iter().any(|finding| finding.lint == UNUSED_VAR));
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.suppression_files_refiltered > 0);
}

#[test]
fn reinferred_consumer_refreshes_when_a_callee_changes() {
    let main = "fn main() -> int { let _value: int = cast(maybe()) 3 }\n";
    let callee_old = "inline(lua) fn cast<T>(value: any) -> T = \"return value\"\nfn maybe() -> int { return 1 }\n";
    let callee_new = "inline(lua) fn cast<T>(value: any) -> T = \"return value\"\nfn maybe() -> int? { return 1 }\n";
    let options = LintOptions::default();
    let mut cache = seeded_cache(
        &[("<dep-main>", main), ("<dep-callee>", callee_old)],
        &options,
    );
    let (findings, stats) = body_edit(
        &mut cache,
        &[("<dep-main>", main), ("<dep-callee>", callee_new)],
        &options,
        &["<dep-callee>"],
        Some("main"),
        None,
    );
    assert!(
        findings
            .iter()
            .any(|f| f.lint == bone_lints::TRANSMUTE_OF_NULLABLE)
    );
    assert!(stats.inference_body_requests > 0);
}

#[test]
fn deprecated_findings_refresh_in_reinferred_consumers() {
    let main = "fn main() { old() }\n";
    let before = "fn old() {}\n";
    let after = "@deprecated(\"use fresh\")\nfn old() {}\n";
    let options = LintOptions::default();
    let mut cache = seeded_cache(&[("<dep-main>", main), ("<dep-callee>", before)], &options);
    let (findings, stats) = body_edit(
        &mut cache,
        &[("<dep-main>", main), ("<dep-callee>", after)],
        &options,
        &["<dep-callee>"],
        Some("main"),
        None,
    );
    assert_eq!(
        findings.iter().filter(|f| f.lint == "deprecated").count(),
        1
    );
    assert!(stats.inference_body_requests > 0);
    let (findings, _) = body_edit(
        &mut cache,
        &[("<dep-main>", main), ("<dep-callee>", before)],
        &options,
        &["<dep-callee>"],
        Some("main"),
        None,
    );
    assert!(findings.iter().all(|f| f.lint != "deprecated"));
}

#[test]
fn production_only_cache_scans_but_does_not_report_test_files() {
    let parts = [
        ("<production>", "fn main() {}\n"),
        ("<sim>", "fn sim_only() {}\n"),
    ];
    let (ws, mut sources) = support::setup(&parts);
    sources.remove(&Ustr::from("<sim>"));
    let mut cache = IncrementalLintCache::default();
    let (findings, stats) = run_update(
        &mut cache,
        &ws,
        &sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    assert_matches_fresh(&findings, &ws, &sources, &LintOptions::default());
    assert!(findings.iter().all(|f| f.file() != Ustr::from("<sim>")));
    assert!(stats.full_rebuilds > 0);
    assert_eq!(stats.suppression_entry_parses, 1);
}

#[test]
fn cached_findings_match_fresh_after_body_edit_and_span_shift() {
    let old = "fn first() {\n    let first_unused = 1\n}\nfn second() {\n    let second_unused = 2\n}\nfn main() { first(); second() }\n";
    let new = "fn first() {\n    let first_unused = 1\n}\n// inserted before an unchanged finding\nfn second() {\n    let second_unused = 2\n}\nfn main() { first(); second() }\n";
    let mut cache = IncrementalLintCache::default();
    let (ws, sources) = support::setup(&[(FILE, old)]);
    let (_, initial_stats) = run_update(
        &mut cache,
        &ws,
        &sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    assert_eq!(initial_stats.full_rebuilds, 1);
    assert!(initial_stats.declarations_revisited > 0);

    let insertion = old.find("fn second").expect("second function");
    let (_, stats) = edit(
        &mut cache,
        new,
        Some("first"),
        Some(shift(insertion, old, new, 1)),
    );
    assert!(stats.body_updates > 0);
    assert_eq!(stats.full_rebuilds, 0);
    assert!(stats.declarations_reused > 0);
    assert!(stats.flat_body_requests > 0);
    assert!(stats.flat_bodies_reused > 0);
    assert!(stats.span_shifts > 0, "cached unchanged finding must shift");
}

#[test]
fn suppression_change_rebuilds_and_finding_free_entries_are_not_stale() {
    let old = "fn main() {\n    let used = 1\n    print(used)\n}\n";
    let new = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let mut cache = seed(old);
    let (cached, stats) = edit(
        &mut cache,
        new,
        Some("main"),
        Some(shift(old.len() / 2, old, new, 1)),
    );
    assert!(stats.suppression_invalidations > 0);
    assert!(
        cached
            .iter()
            .all(|finding| finding.lint != bone_lints::UNUSED_VAR)
    );
}

#[test]
fn invalidated_file_reloads_its_local_suppressions_before_body_derivation() {
    let src = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let mut cache = seed(src);
    cache.invalidate_file(Ustr::from(FILE));
    let (cached, stats) = edit(&mut cache, src, Some("main"), None);
    assert!(cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.suppression_entry_parses, 1);
}

#[test]
fn invalidated_file_reloads_suppressions_before_global_derivation() {
    let mut cache = seed("// rock: allow(unused_fn)\nfn helper() {}\nfn main() { helper() }\n");
    cache.invalidate_file(Ustr::from(FILE));
    let new = "// rock: allow(unused_fn)\nfn helper() {}\nfn main() {}\n";
    let (cached, stats) = edit(&mut cache, new, Some("main"), None);
    assert!(cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.suppression_entry_parses, 1);
}

#[test]
fn action_caller_addition_and_removal_are_incremental_when_opted_in() {
    let uncalled =
        "global ACT_USED: int = 1\nfn gameplay() { let value = 0 }\nfn main() { gameplay() }\n";
    let called = "global ACT_USED: int = 1\nfn gameplay() { let value = ACT_USED }\nfn main() { gameplay() }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, uncalled)], &selection);
    for (src, expected) in [(called, 0), (uncalled, 1)] {
        let (cached, stats) = body_edit(
            &mut cache,
            &[(ACTION_FILE, src)],
            &selection,
            &[ACTION_FILE],
            Some("gameplay"),
            None,
        );
        assert_eq!(cached.len(), expected);
        assert!(cached.iter().all(|f| f.error.message.contains("ACT_USED")));
        assert_eq!(stats.full_rebuilds, 0);
        assert_eq!(stats.global_passes, 0);
    }
}

#[test]
fn backend_sibling_signature_change_revisits_only_its_group() {
    let old = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\nfn main() {}\n";
    let new = old.replace(
        "inline(js) fn probe(x: int)",
        "inline(js) fn probe(x: float)",
    );
    let selection = sibling_selection();
    let mut cache = seeded_cache(&[(FILE, old)], &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &[(FILE, &new)],
        &selection,
        &[FILE],
        Some("main"),
        None,
    );
    assert!(
        cached
            .iter()
            .any(|finding| finding.lint == bone_lints::BACKEND_SIBLING_MISMATCH)
    );
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.sibling_groups_requested > 0);
    assert!(stats.sibling_groups_revisited > 0);
}

#[test]
fn unselected_backend_sibling_change_revisits_no_group() {
    let old = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\nfn main() {}\n";
    let new = old.replace(
        "inline(js) fn probe(x: int)",
        "inline(js) fn probe(x: float)",
    );
    let mut cache = seed(old);
    let (cached, stats) = edit(&mut cache, &new, Some("main"), None);
    assert!(
        cached
            .iter()
            .all(|f| f.lint != bone_lints::BACKEND_SIBLING_MISMATCH)
    );
    assert_eq!(stats.global_passes, 0);
    assert_eq!(stats.sibling_groups_revisited, 0);
}

#[test]
fn backend_sibling_addition_and_removal_update_the_group() {
    let lua = [(
        FILE,
        "inline(lua) fn probe(x: int) -> int = \"return x\"\nfn main() {}\n",
    )];
    let both = [(
        FILE,
        "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: float) -> int = \"return x;\"\nfn main() {}\n",
    )];
    let selection = sibling_selection();
    let mut cache = seeded_cache(&lua, &selection);

    let (added, added_stats) =
        body_edit(&mut cache, &both, &selection, &[FILE], Some("main"), None);
    assert!(!added.is_empty());
    assert!(
        added
            .iter()
            .all(|finding| finding.lint == bone_lints::BACKEND_SIBLING_MISMATCH)
    );
    assert_eq!(added_stats.full_rebuilds, 0);

    let (removed, removed_stats) = body_edit(&mut cache, &lua, &selection, &[FILE], None, None);
    assert!(removed.is_empty());
    assert_eq!(removed_stats.full_rebuilds, 0);
}

#[test]
fn backend_sibling_file_movement_revisits_old_and_new_groups() {
    let old_parts = [
        (
            "<sibling-a>",
            "inline(lua) fn probe(x: int) -> int = \"return x\"\nfn main() {}\n",
        ),
        (
            "<sibling-b>",
            "inline(js) fn probe(x: int) -> int = \"return x;\"\n",
        ),
    ];
    let new_parts = [
        (
            "<sibling-a>",
            "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: float) -> int = \"return x;\"\nfn main() {}\n",
        ),
        ("<sibling-b>", "fn helper() {}\n"),
    ];
    let selection = sibling_selection();
    let mut cache = seeded_cache(&old_parts, &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &new_parts,
        &selection,
        &["<sibling-a>", "<sibling-b>"],
        Some("main"),
        None,
    );
    assert!(
        cached
            .iter()
            .any(|finding| finding.lint == bone_lints::BACKEND_SIBLING_MISMATCH)
    );
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.sibling_groups_revisited > 0);
    assert!(
        cached
            .iter()
            .all(|finding| finding.file() == Ustr::from("<sibling-a>"))
    );
}

#[test]
fn backend_sibling_portable_replacement_revisits_the_serving_group() {
    let old = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\nfn probe(x: float) -> int { return 0 }\nfn main() {}\n";
    let new = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\ninline(lua) fn probe(x: float) -> int = \"return x\"\nfn main() {}\n";
    let selection = sibling_selection();
    let mut cache = seeded_cache(&[(FILE, old)], &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &[(FILE, new)],
        &selection,
        &[FILE],
        Some("main"),
        None,
    );
    assert!(!cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.sibling_groups_revisited > 0);
}

#[test]
fn action_declaration_addition_and_removal_update_the_empty_population() {
    let empty = "fn main() { let value = 0 }\n";
    let declared = "global ACT_NEW: int = 1\nfn main() { let value = ACT_NEW }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, empty)], &selection);
    for (src, expected) in [(declared, 0), (empty, 1)] {
        let (cached, stats) = body_edit(
            &mut cache,
            &[(ACTION_FILE, src)],
            &selection,
            &[ACTION_FILE],
            Some("main"),
            None,
        );
        assert_eq!(cached.len(), expected);
        assert!(
            cached
                .iter()
                .all(|f| f.error.message.contains("no production App ACT_*"))
        );
        assert_eq!(stats.full_rebuilds, 0);
        assert_eq!(stats.global_passes, 0);
    }
}

#[test]
fn action_population_elected_in_an_unchanged_file_is_published() {
    let old_parts = [
        ("app/other.rock", "fn helper() {}\n"),
        (
            ACTION_FILE,
            "global ACT_OLD: int = 1\nfn main() { let value = ACT_OLD }\n",
        ),
    ];
    let new_parts = [
        ("app/other.rock", "fn helper() {}\n"),
        (ACTION_FILE, "fn main() { let value = 0 }\n"),
    ];
    let selection = action_selection();
    let mut cache = seeded_cache(&old_parts, &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &new_parts,
        &selection,
        &[ACTION_FILE],
        Some("main"),
        None,
    );
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].file(), Ustr::from("app/other.rock"));
    assert_eq!(stats.full_rebuilds, 0);
}

#[test]
fn declaration_refresh_replaces_raw_global_rows_before_suppression_rollback() {
    let old = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: float) -> int = \"return x;\"\nfn main() {}\n";
    let matched = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\nfn main() {}\n";
    let suppressed = matched
        .replace(
            "inline(lua)",
            "// rock: allow(backend_sibling_mismatch)\ninline(lua)",
        )
        .replace(
            "inline(js)",
            "// rock: allow(backend_sibling_mismatch)\ninline(js)",
        );
    let selection = sibling_selection();
    let mut cache = seeded_cache(&[(FILE, old)], &selection);
    let mut step = |src: &str, function: Option<&str>, edit: Option<TextEditDelta>| {
        let (findings, _) = body_edit(
            &mut cache,
            &[(FILE, src)],
            &selection,
            &[FILE],
            function,
            edit,
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.lint != bone_lints::BACKEND_SIBLING_MISMATCH)
        );
    };
    step(matched, Some("main"), None);
    let insertion = suppressed.find("// rock").expect("allow");
    step(
        &suppressed,
        None,
        Some(shift(insertion, matched, &suppressed, 2)),
    );
    step(
        matched,
        None,
        Some(shift(insertion, &suppressed, matched, -2)),
    );
}

#[test]
fn unused_fn_declaration_addition_and_removal_update_only_its_component() {
    let old = "fn main() {}\n";
    let new = "fn helper() {}\nfn main() {}\n";
    let mut cache = seed(old);
    let (cached, stats) = edit(&mut cache, new, Some("main"), None);
    assert!(
        cached
            .iter()
            .any(|finding| finding.error.message.contains("helper"))
    );
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.changed_declarations > 0);
    assert!(stats.reachability_components_visited > 0);

    let (cached, stats) = edit(&mut cache, old, Some("main"), None);
    assert!(
        cached
            .iter()
            .all(|finding| !finding.error.message.contains("helper"))
    );
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.changed_declarations > 0);
}

#[test]
fn inference_error_is_added_and_removed_from_the_changed_body() {
    let prelude = r#"
inline(lua) fn cast<T>(value: any) -> T = "return value"
fn maybe_int() -> int? { return 1 }
fn always_int() -> int { return 1 }
"#;
    let good =
        format!("{prelude}\nfn main() -> int {{ let _value: int = cast(always_int()) 3 }}\n");
    let bad = format!("{prelude}\nfn main() -> int {{ let _value: int = cast(maybe_int()) 3 }}\n");
    let mut cache = seed(&good);
    let (bad_findings, _) = edit(&mut cache, &bad, Some("main"), None);
    assert!(
        bad_findings
            .iter()
            .any(|f| f.lint == bone_lints::TRANSMUTE_OF_NULLABLE)
    );

    let (fixed_findings, stats) = edit(&mut cache, &good, Some("main"), None);
    assert!(
        fixed_findings
            .iter()
            .all(|f| f.lint != bone_lints::TRANSMUTE_OF_NULLABLE)
    );
    assert!(stats.inference_body_requests > 0);
}

/// Each case edits only `main`'s body. The cache must agree with a fresh lint
/// without a full rebuild or a workspace pass, flag exactly the `dead` names
/// among those listed, and touch the call graph in the stated direction.
#[test]
fn unused_fn_caller_edits_update_only_affected_reachability() {
    struct Case {
        name: &'static str,
        old: &'static str,
        new: &'static str,
        dead: &'static [&'static str],
        live: &'static [&'static str],
        adds_edge: bool,
    }
    let cases = [
        Case {
            name: "adding the last caller",
            old: "fn helper() {}\nfn other_dead() {}\nfn main() {}\n",
            new: "fn helper() {}\nfn other_dead() {}\nfn main() { helper() }\n",
            dead: &["other_dead"],
            live: &["helper"],
            adds_edge: true,
        },
        Case {
            name: "removing the last caller",
            old: "fn helper() {}\nfn other_dead() {}\nfn main() { helper() }\n",
            new: "fn helper() {}\nfn other_dead() {}\nfn main() {}\n",
            dead: &["helper", "other_dead"],
            live: &[],
            adds_edge: false,
        },
        Case {
            name: "removing the root of a cycle",
            old: "fn a() { b() }\nfn b() { a() }\nfn main() { a() }\n",
            new: "fn a() { b() }\nfn b() { a() }\nfn main() {}\n",
            dead: &["a", "b"],
            live: &[],
            adds_edge: false,
        },
        Case {
            name: "adding a root to a cycle",
            old: "fn a() { b() }\nfn b() { a() }\nfn main() {}\n",
            new: "fn a() { b() }\nfn b() { a() }\nfn main() { a() }\n",
            dead: &[],
            live: &["a", "b"],
            adds_edge: true,
        },
        Case {
            name: "a generic callee resolves through its specialization",
            old: "fn identity<T>(value: T) -> T { value }\nfn main() {}\n",
            new: "fn identity<T>(value: T) -> T { value }\nfn main() { identity(1) }\n",
            dead: &[],
            live: &["identity"],
            adds_edge: true,
        },
        Case {
            name: "a UFCS call resolves to its method",
            old: "struct Mob { hp: int }\nfn Mob.damage(m, amount: int) { m.hp -= amount }\nfn main() {}\n",
            new: "struct Mob { hp: int }\nfn Mob.damage(m, amount: int) { m.hp -= amount }\nfn main() { Mob { hp: 1 }.damage(1) }\n",
            dead: &[],
            live: &["damage"],
            adds_edge: true,
        },
    ];
    for case in cases {
        let name = case.name;
        let mut cache = seed(case.old);
        let (cached, stats) = edit(&mut cache, case.new, Some("main"), None);
        let unused: Vec<String> = cached
            .iter()
            .filter(|finding| finding.lint == bone_lints::UNUSED_FN)
            .map(support::quoted)
            .collect();
        for dead in case.dead {
            assert!(
                unused.iter().any(|n| n == dead),
                "{name}: {dead} must be flagged: {unused:?}"
            );
        }
        for live in case.live {
            assert!(
                unused.iter().all(|n| !n.contains(live)),
                "{name}: {live} must not be flagged: {unused:?}"
            );
        }
        assert_eq!(stats.full_rebuilds, 0, "{name}");
        assert_eq!(
            stats.global_passes, 0,
            "{name}: must not refresh the workspace"
        );
        assert!(stats.reachability_components_visited > 0, "{name}");
        if case.adds_edge {
            assert!(stats.call_edges_installed > 0, "{name}");
        } else {
            assert!(stats.call_edges_retracted > 0, "{name}");
        }
    }
}

#[test]
fn file_set_change_takes_the_explicit_full_reconstruction_route() {
    let options = LintOptions::default();
    let mut cache = seeded_cache(&[(FILE, "fn main() {}\n")], &options);
    let new = [
        (FILE, "fn main() {}\n"),
        ("<incremental-cache-2>", "fn helper() {}\n"),
    ];
    let (_, stats) = body_edit(&mut cache, &new, &options, &[FILE], None, None);
    assert_eq!(stats.full_rebuilds, 1);
}

#[test]
fn suppression_removal_after_a_suppressed_rebuild_restores_the_finding() {
    let suppressed = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let shown = "fn main() {\n    let unused = 1\n}\n";
    let removal = suppressed.find("// rock").expect("suppression");
    let mut cache = seed(suppressed);
    let (_, stats) = edit(
        &mut cache,
        shown,
        None,
        Some(shift(removal, suppressed, shown, -1)),
    );
    assert_eq!(stats.full_rebuilds, 0);
}

#[test]
fn tied_findings_are_ordered_by_label_spans_in_full_and_cached_lints() {
    let src = "fn main() { let value = 1\n let value = 2\n }\n";
    let parts = [(FILE, src)];
    let options = LintOptions::default();
    for ends in [[19, 18], [18, 19]] {
        let (mut ws, sources) = support::setup(&parts);
        let (mid, owner) = function_owner(&ws, "main");
        let body = ws.module_mut(mid).bodies.get_mut(&owner).expect("body");
        // Simulate findings with the same primary position and message but
        // different extents, in either derivation order.
        for ((_, binding), end) in body.binding_arena.iter_mut().zip(ends) {
            binding.span = Span::new(Ustr::from(FILE), (0, 0), (15, end), (15, end));
        }
        let mut cache = IncrementalLintCache::default();
        let (rebuilt, _) = run_update(
            &mut cache,
            &ws,
            &sources,
            &options,
            &LintCacheUpdate::full(),
        );
        let (updated, stats) = run_update(
            &mut cache,
            &ws,
            &sources,
            &options,
            &LintCacheUpdate::body_only([Ustr::from(FILE)], [(mid, owner)], None),
        );
        assert_eq!(stats.full_rebuilds, 0);
        for findings in [lint_fresh(&ws, &sources, &options), rebuilt, updated] {
            assert_eq!(findings.len(), 2, "{findings:#?}");
            assert_eq!(findings[0].lint, findings[1].lint);
            assert_eq!(findings[0].error.message, findings[1].error.message);
            let spans: Vec<_> = findings
                .iter()
                .map(|finding| finding.error.labels[0].span.offset_range())
                .collect();
            assert_eq!(spans, [(15, 18), (15, 19)]);
        }
    }
}

#[test]
fn cached_and_fresh_findings_share_one_order_after_a_body_update() {
    let src = "fn first(a: int) {\n    a == 1\n    let first_unused = 1\n}\nfn second(b: int) {\n    b == 2\n    let second_unused = 2\n}\nfn main() { first(1); second(2) }\n";
    let mut cache = seed(src);
    let (updated, stats) = edit(&mut cache, src, Some("second"), None);
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(updated.len(), 4, "{updated:#?}");
}

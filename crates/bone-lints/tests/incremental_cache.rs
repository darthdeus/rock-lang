use crate::support;
use bone_lints::{
    Finding, IncrementalLintCache, LintCacheStats, LintCacheUpdate, LintOptions, TextEditDelta,
    UNUSED_VAR, lint_workspace,
};
use bone_semantic::prelude::*;

const FILE: &str = "<incremental-cache>";
const ACTION_FILE: &str = "app/player.rock";

fn workspace(src: &str) -> Workspace {
    let ast = skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse");
    semantic_pass(&mut [ast]).expect("semantic pass")
}

fn workspace_many(srcs: &[(&str, &str)]) -> Workspace {
    let mut asts = srcs
        .iter()
        .map(|(file, src)| skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse"))
        .collect::<Vec<_>>();
    semantic_pass(&mut asts).expect("semantic pass")
}

fn sources(src: &str) -> FxHashMap<Ustr, &str> {
    let mut out = FxHashMap::default();
    out.insert(Ustr::from(FILE), src);
    out
}

fn sources_many<'a>(srcs: &'a [(&'a str, &'a str)]) -> FxHashMap<Ustr, &'a str> {
    srcs.iter()
        .map(|(file, src)| (Ustr::from(file), *src))
        .collect()
}

fn run_update(
    cache: &mut IncrementalLintCache,
    ws: &Workspace,
    sources: &FxHashMap<Ustr, &str>,
    options: &LintOptions,
    change: &LintCacheUpdate,
) -> (Vec<Finding>, LintCacheStats) {
    cache.update(ws, &FxHashSet::default(), true, sources, options, change)
}

fn lint_fresh(
    ws: &Workspace,
    sources: &FxHashMap<Ustr, &str>,
    options: &LintOptions,
) -> Vec<Finding> {
    lint_workspace(ws, &FxHashSet::default(), true, sources, options)
}

fn shapes(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|finding| {
            let span = finding.error.labels.first().map(|label| label.span);
            let offsets = span.map(|span| span.offset_range());
            format!(
                "{}:{}:{span:?}:{offsets:?}",
                finding.lint, finding.error.message
            )
        })
        .collect()
}

fn function_owner(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    function_owner_in(ws, FILE, name)
}

fn function_owner_in(ws: &Workspace, file: &str, name: &str) -> (ModuleId, BodyOwner) {
    let module = ws
        .modules
        .iter()
        .find(|m| m.file == Ustr::from(file))
        .expect("module");
    let (fid, _) = module
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == name)
        .expect("function");
    (module.id, BodyOwner::Function(fid))
}

fn action_selection() -> LintOptions {
    LintOptions::only([bone_lints::ACTION_CALLERS.to_owned()])
        .with_action_policy(Some(&support::POLICY))
}

fn sibling_selection() -> LintOptions {
    LintOptions::only([bone_lints::BACKEND_SIBLING_MISMATCH.to_owned()])
}

fn seeded_cache(parts: &[(&str, &str)], selection: &LintOptions) -> IncrementalLintCache {
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &workspace_many(parts),
        &sources_many(parts),
        selection,
        &LintCacheUpdate::full(),
    );
    cache
}

/// Move `cache` to `parts` as a body edit of `changed`, re-inferring
/// `function` in the first changed file, and check it against a fresh lint.
fn body_edit(
    cache: &mut IncrementalLintCache,
    parts: &[(&str, &str)],
    selection: &LintOptions,
    changed: &[&str],
    function: Option<&str>,
) -> (Vec<Finding>, LintCacheStats) {
    let ws = workspace_many(parts);
    let sources = sources_many(parts);
    let reinferred = function.map(|name| function_owner_in(&ws, changed[0], name));
    let change = LintCacheUpdate::body_only(changed.iter().map(|f| Ustr::from(f)), reinferred, []);
    let (cached, stats) = run_update(cache, &ws, &sources, selection, &change);
    assert_eq!(
        shapes(&cached),
        shapes(&lint_fresh(&ws, &sources, selection))
    );
    (cached, stats)
}

#[test]
fn local_body_findings_can_be_added_and_removed() {
    let old = "fn main() { let used = 1\n print(used) }\n";
    let added = "fn main() { let unused = 1\n let used = 1\n print(used) }\n";
    let removed = "fn main() { print(1) }\n";
    let old_ws = workspace(old);
    let added_ws = workspace(added);
    let removed_ws = workspace(removed);
    let file = Ustr::from(FILE);
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );

    let (added_mid, added_owner) = function_owner(&added_ws, "main");
    let (added_findings, added_stats) = run_update(
        &mut cache,
        &added_ws,
        &sources(added),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(added_mid, added_owner)], []),
    );
    assert!(added_findings.iter().any(|f| f.lint == UNUSED_VAR));
    assert!(added_stats.flat_body_requests > 0);

    let (removed_mid, removed_owner) = function_owner(&removed_ws, "main");
    let (removed_findings, removed_stats) = run_update(
        &mut cache,
        &removed_ws,
        &sources(removed),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(removed_mid, removed_owner)], []),
    );
    assert!(removed_findings.iter().all(|f| f.lint != UNUSED_VAR));
    assert!(removed_stats.flat_body_requests > 0);
}

#[test]
fn suppression_only_edits_rebuild_the_affected_file() {
    let old = "fn main() {\n    let unused = 1\n}\n";
    let suppressed = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let old_ws = workspace(old);
    let suppressed_ws = workspace(suppressed);
    let file = Ustr::from(FILE);
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let insertion = suppressed.find("// rock").expect("suppression");
    let (mid, owner) = function_owner(&suppressed_ws, "main");
    let (hidden, hidden_stats) = run_update(
        &mut cache,
        &suppressed_ws,
        &sources(suppressed),
        &LintOptions::default(),
        &LintCacheUpdate::body_only(
            [file],
            [],
            [TextEditDelta {
                file,
                at: insertion,
                byte_delta: (suppressed.len() - old.len()) as isize,
                line_delta: 1,
            }],
        ),
    );
    assert!(hidden.is_empty());
    assert_eq!(hidden_stats.full_rebuilds, 0);
    assert!(hidden_stats.suppression_invalidations > 0);
    assert_eq!(hidden_stats.flat_body_requests, 0);
    assert!(hidden_stats.suppression_files_refiltered > 0);

    let (shown, shown_stats) = run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    assert!(shown.iter().any(|f| f.lint == UNUSED_VAR));
    assert_eq!(shown_stats.full_rebuilds, 0);
    assert!(shown_stats.suppression_files_refiltered > 0);
}

#[test]
fn reinferred_consumer_refreshes_when_a_callee_changes() {
    let main = "fn main() -> int { let _value: int = cast(maybe()) 3 }\n";
    let callee_old = "inline(lua) fn cast<T>(value: any) -> T = \"return value\"\nfn maybe() -> int { return 1 }\n";
    let callee_new = "inline(lua) fn cast<T>(value: any) -> T = \"return value\"\nfn maybe() -> int? { return 1 }\n";
    let old_parts = [("<dep-main>", main), ("<dep-callee>", callee_old)];
    let new_parts = [("<dep-main>", main), ("<dep-callee>", callee_new)];
    let old_ws = workspace_many(&old_parts);
    let new_ws = workspace_many(&new_parts);
    let old_sources = sources_many(&old_parts);
    let new_sources = sources_many(&new_parts);
    let callee_file = Ustr::from("<dep-callee>");
    let (main_mid, main_owner) = function_owner_in(&new_ws, "<dep-main>", "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (findings, stats) = run_update(
        &mut cache,
        &new_ws,
        &new_sources,
        &LintOptions::default(),
        &LintCacheUpdate::body_only([callee_file], [(main_mid, main_owner)], []),
    );
    let fresh = lint_fresh(&new_ws, &new_sources, &LintOptions::default());
    assert_eq!(shapes(&findings), shapes(&fresh));
    assert!(
        findings
            .iter()
            .any(|f| f.lint == bone_lints::TRANSMUTE_OF_NULLABLE)
    );
    assert!(stats.inference_body_requests > 0);
}

#[test]
fn production_only_cache_scans_but_does_not_report_test_files() {
    let parts = [
        ("<production>", "fn main() {}\n"),
        ("<sim>", "fn sim_only() {}\n"),
    ];
    let ws = workspace_many(&parts);
    let production = [parts[0]];
    let sources = sources_many(&production);
    let mut cache = IncrementalLintCache::default();
    let (findings, stats) = run_update(
        &mut cache,
        &ws,
        &sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let fresh = lint_fresh(&ws, &sources, &LintOptions::default());
    assert_eq!(shapes(&findings), shapes(&fresh));
    assert!(findings.iter().all(|f| f.file() != Ustr::from("<sim>")));
    assert!(stats.full_rebuilds > 0);
    assert_eq!(stats.suppression_entry_parses, 1);
}

#[test]
fn cached_findings_match_fresh_after_body_edit_and_span_shift() {
    let old = "fn first() {\n    let first_unused = 1\n}\nfn second() {\n    let second_unused = 2\n}\nfn main() { first(); second() }\n";
    let new = "fn first() {\n    let first_unused = 1\n}\n// inserted before an unchanged finding\nfn second() {\n    let second_unused = 2\n}\nfn main() { first(); second() }\n";
    let old_ws = workspace(old);
    let new_ws = workspace(new);
    let file = Ustr::from(FILE);
    let mut cache = IncrementalLintCache::default();
    let old_sources = sources(old);
    let (_, initial_stats) = run_update(
        &mut cache,
        &old_ws,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    assert_eq!(initial_stats.full_rebuilds, 1);
    assert!(initial_stats.declarations_revisited > 0);

    let (mid, owner) = function_owner(&new_ws, "first");
    let insertion = old.find("fn second").expect("second function");
    let update = LintCacheUpdate::body_only(
        [file],
        [(mid, owner)],
        [TextEditDelta {
            file,
            at: insertion,
            byte_delta: (new.len() - old.len()) as isize,
            line_delta: 1,
        }],
    );
    let new_sources = sources(new);
    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &new_sources,
        &LintOptions::default(),
        &update,
    );
    let fresh = lint_fresh(&new_ws, &new_sources, &LintOptions::default());

    assert_eq!(format!("{cached:?}"), format!("{fresh:?}"));
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
    let old_ws = workspace(old);
    let new_ws = workspace(new);
    let file = Ustr::from(FILE);
    let mut cache = IncrementalLintCache::default();
    let old_sources = sources(old);
    run_update(
        &mut cache,
        &old_ws,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (mid, owner) = function_owner(&new_ws, "main");
    let new_sources = sources(new);
    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &new_sources,
        &LintOptions::default(),
        &LintCacheUpdate::body_only(
            [file],
            [(mid, owner)],
            [TextEditDelta {
                file,
                at: old.len() / 2,
                byte_delta: (new.len() - old.len()) as isize,
                line_delta: 1,
            }],
        ),
    );
    let fresh = lint_fresh(&new_ws, &new_sources, &LintOptions::default());

    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(stats.suppression_invalidations > 0);
    assert!(
        cached
            .iter()
            .all(|finding| finding.lint != bone_lints::UNUSED_VAR)
    );
}

#[test]
fn suppression_add_and_remove_refilters_without_rederiving_findings() {
    let old = "fn main() {\n    let unused = 1\n}\n";
    let suppressed = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let old_ws = workspace(old);
    let suppressed_ws = workspace(suppressed);
    let file = Ustr::from(FILE);
    let insertion = suppressed.find("// rock").expect("suppression");
    let (suppressed_mid, suppressed_owner) = function_owner(&suppressed_ws, "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );

    let (hidden, hidden_stats) = run_update(
        &mut cache,
        &suppressed_ws,
        &sources(suppressed),
        &LintOptions::default(),
        &LintCacheUpdate::body_only(
            [file],
            [],
            [TextEditDelta {
                file,
                at: insertion,
                byte_delta: (suppressed.len() - old.len()) as isize,
                line_delta: 1,
            }],
        ),
    );
    let hidden_fresh = lint_fresh(
        &suppressed_ws,
        &sources(suppressed),
        &LintOptions::default(),
    );
    assert_eq!(shapes(&hidden), shapes(&hidden_fresh));
    assert!(hidden.is_empty());
    assert_eq!(hidden_stats.full_rebuilds, 0);
    assert_eq!(hidden_stats.flat_body_requests, 0);
    assert_eq!(hidden_stats.global_passes, 0);

    let (shown, shown_stats) = run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::body_only(
            [file],
            [(suppressed_mid, suppressed_owner)],
            [TextEditDelta {
                file,
                at: insertion,
                byte_delta: -((suppressed.len() - old.len()) as isize),
                line_delta: -1,
            }],
        ),
    );
    let shown_fresh = lint_fresh(&old_ws, &sources(old), &LintOptions::default());
    assert_eq!(shapes(&shown), shapes(&shown_fresh));
    assert!(shown.iter().any(|finding| finding.lint == UNUSED_VAR));
    assert_eq!(shown_stats.full_rebuilds, 0);
    assert_eq!(shown_stats.global_passes, 0);
}

#[test]
fn invalidated_file_reloads_its_local_suppressions_before_body_derivation() {
    let src = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let ws = workspace(src);
    let file = Ustr::from(FILE);
    let (mid, owner) = function_owner(&ws, "main");
    let mut cache = IncrementalLintCache::default();
    let (initial, _) = run_update(
        &mut cache,
        &ws,
        &sources(src),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    assert!(initial.is_empty());

    cache.invalidate_file(file);
    let (cached, stats) = run_update(
        &mut cache,
        &ws,
        &sources(src),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_fresh(&ws, &sources(src), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.suppression_entry_parses, 1);
}

#[test]
fn invalidated_file_reloads_suppressions_before_global_derivation() {
    let old = "// rock: allow(unused_fn)\nfn helper() {}\nfn main() { helper() }\n";
    let new = "// rock: allow(unused_fn)\nfn helper() {}\nfn main() {}\n";
    let old_ws = workspace(old);
    let new_ws = workspace(new);
    let file = Ustr::from(FILE);
    let (mid, owner) = function_owner(&new_ws, "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );

    cache.invalidate_file(file);
    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &sources(new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_fresh(&new_ws, &sources(new), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.suppression_entry_parses, 1);
}

#[test]
fn action_caller_addition_is_incremental_when_opted_in() {
    let old =
        "global ACT_USED: int = 1\nfn gameplay() { let value = 0 }\nfn main() { gameplay() }\n";
    let new = "global ACT_USED: int = 1\nfn gameplay() { let value = ACT_USED }\nfn main() { gameplay() }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, old)], &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &[(ACTION_FILE, new)],
        &selection,
        &[ACTION_FILE],
        Some("gameplay"),
    );
    assert!(cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
}

#[test]
fn action_caller_removal_is_incremental_when_opted_in() {
    let old = "global ACT_USED: int = 1\nfn gameplay() { let value = ACT_USED }\nfn main() { gameplay() }\n";
    let new =
        "global ACT_USED: int = 1\nfn gameplay() { let value = 0 }\nfn main() { gameplay() }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, old)], &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &[(ACTION_FILE, new)],
        &selection,
        &[ACTION_FILE],
        Some("gameplay"),
    );
    assert_eq!(cached.len(), 1);
    assert!(cached[0].error.message.contains("ACT_USED"));
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
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

    let (added, added_stats) = body_edit(&mut cache, &both, &selection, &[FILE], Some("main"));
    assert!(!added.is_empty());
    assert!(
        added
            .iter()
            .all(|finding| finding.lint == bone_lints::BACKEND_SIBLING_MISMATCH)
    );
    assert_eq!(added_stats.full_rebuilds, 0);

    let (removed, removed_stats) = body_edit(&mut cache, &lua, &selection, &[FILE], None);
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
    );
    assert!(!cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.sibling_groups_revisited > 0);
}

#[test]
fn action_declaration_addition_updates_the_empty_population() {
    let old = "fn main() {}\n";
    let new = "global ACT_NEW: int = 1\nfn main() { let value = ACT_NEW }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, old)], &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &[(ACTION_FILE, new)],
        &selection,
        &[ACTION_FILE],
        Some("main"),
    );
    assert!(cached.is_empty());
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
}

#[test]
fn action_declaration_removal_restores_the_empty_population() {
    let old = "global ACT_OLD: int = 1\nfn main() { let value = ACT_OLD }\n";
    let new = "fn main() { let value = 0 }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, old)], &selection);
    let (cached, stats) = body_edit(
        &mut cache,
        &[(ACTION_FILE, new)],
        &selection,
        &[ACTION_FILE],
        Some("main"),
    );
    assert_eq!(cached.len(), 1);
    assert!(cached[0].error.message.contains("no production App ACT_*"));
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
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
    let old_ws = workspace(old);
    let matched_ws = workspace(matched);
    let suppressed_ws = workspace(&suppressed);
    let file = Ustr::from(FILE);
    let selection = LintOptions::only([bone_lints::BACKEND_SIBLING_MISMATCH.to_owned()]);
    let (main_mid, main_owner) = function_owner(&matched_ws, "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &selection,
        &LintCacheUpdate::full(),
    );

    let (matched_findings, _) = run_update(
        &mut cache,
        &matched_ws,
        &sources(matched),
        &selection,
        &LintCacheUpdate::body_only([file], [(main_mid, main_owner)], []),
    );
    let matched_fresh = lint_fresh(&matched_ws, &sources(matched), &selection);
    assert_eq!(shapes(&matched_findings), shapes(&matched_fresh));
    assert!(
        matched_findings
            .iter()
            .all(|finding| { finding.lint != bone_lints::BACKEND_SIBLING_MISMATCH })
    );

    let insertion = suppressed.find("// rock").expect("allow");
    let (hidden, _) = run_update(
        &mut cache,
        &suppressed_ws,
        &sources(&suppressed),
        &selection,
        &LintCacheUpdate::body_only(
            [file],
            [],
            [TextEditDelta {
                file,
                at: insertion,
                byte_delta: (suppressed.len() - matched.len()) as isize,
                line_delta: 2,
            }],
        ),
    );
    assert!(
        hidden
            .iter()
            .all(|finding| { finding.lint != bone_lints::BACKEND_SIBLING_MISMATCH })
    );

    let (rolled_back, _) = run_update(
        &mut cache,
        &matched_ws,
        &sources(matched),
        &selection,
        &LintCacheUpdate::body_only(
            [file],
            [],
            [TextEditDelta {
                file,
                at: insertion,
                byte_delta: -((suppressed.len() - matched.len()) as isize),
                line_delta: -2,
            }],
        ),
    );
    let rolled_back_fresh = lint_fresh(&matched_ws, &sources(matched), &selection);
    assert_eq!(shapes(&rolled_back), shapes(&rolled_back_fresh));
}

#[test]
fn unused_fn_declaration_addition_updates_only_its_component() {
    let old = "fn main() {}\n";
    let new = "fn helper() {}\nfn main() {}\n";
    let old_ws = workspace(old);
    let new_ws = workspace(new);
    let file = Ustr::from(FILE);
    let (main_mid, main_owner) = function_owner(&new_ws, "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );

    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &sources(new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(main_mid, main_owner)], []),
    );
    let fresh = lint_fresh(&new_ws, &sources(new), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(
        cached
            .iter()
            .any(|finding| finding.error.message.contains("helper"))
    );
    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(stats.global_passes, 0);
    assert!(stats.changed_declarations > 0);
    assert!(stats.reachability_components_visited > 0);
}

#[test]
fn unused_fn_declaration_removal_removes_its_last_finding() {
    let old = "fn helper() {}\nfn main() {}\n";
    let new = "fn main() {}\n";
    let old_ws = workspace(old);
    let new_ws = workspace(new);
    let file = Ustr::from(FILE);
    let (main_mid, main_owner) = function_owner(&new_ws, "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );

    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &sources(new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(main_mid, main_owner)], []),
    );
    let fresh = lint_fresh(&new_ws, &sources(new), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
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
    let old = format!("{prelude}\nfn main() -> int {{ let _value: int = cast(always_int()) 3 }}\n");
    let bad = format!("{prelude}\nfn main() -> int {{ let _value: int = cast(maybe_int()) 3 }}\n");
    let fixed = old.clone();
    let old_ws = workspace(&old);
    let bad_ws = workspace(&bad);
    let file = Ustr::from(FILE);
    let (mid, owner) = function_owner(&bad_ws, "main");
    let mut cache = IncrementalLintCache::default();
    let old_sources = sources(&old);
    run_update(
        &mut cache,
        &old_ws,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (bad_findings, _) = run_update(
        &mut cache,
        &bad_ws,
        &sources(&bad),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    assert!(
        bad_findings
            .iter()
            .any(|f| f.lint == bone_lints::TRANSMUTE_OF_NULLABLE)
    );

    let fixed_ws = workspace(&fixed);
    let (fixed_mid, fixed_owner) = function_owner(&fixed_ws, "main");
    let (fixed_findings, stats) = run_update(
        &mut cache,
        &fixed_ws,
        &sources(&fixed),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(fixed_mid, fixed_owner)], []),
    );
    let fresh = lint_fresh(&fixed_ws, &sources(&fixed), &LintOptions::default());
    assert_eq!(shapes(&fixed_findings), shapes(&fresh));
    assert!(
        fixed_findings
            .iter()
            .all(|f| f.lint != bone_lints::TRANSMUTE_OF_NULLABLE)
    );
    assert!(stats.inference_body_requests > 0);
}

#[test]
fn global_call_edge_and_backend_sibling_changes_do_not_serve_stale_findings() {
    let old = "fn helper() {}\nfn main() {}\n";
    let new = "fn helper() {}\nfn main() { helper() }\n";
    let old_ws = workspace(old);
    let new_ws = workspace(new);
    let file = Ustr::from(FILE);
    let (mid, owner) = function_owner(&new_ws, "main");
    let mut cache = IncrementalLintCache::default();
    let old_sources = sources(old);
    run_update(
        &mut cache,
        &old_ws,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let new_sources = sources(new);
    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &new_sources,
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_fresh(&new_ws, &new_sources, &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(cached.iter().all(|f| f.lint != bone_lints::UNUSED_FN));
    assert_eq!(stats.global_passes, 0);

    let sibling_old = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\nfn main() {}\n";
    let sibling_new = sibling_old.replace(
        "inline(js) fn probe(x: int)",
        "inline(js) fn probe(x: float)",
    );
    let old_ws = workspace(sibling_old);
    let new_ws = workspace(&sibling_new);
    let (mid, owner) = function_owner(&new_ws, "main");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources(sibling_old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (cached, sibling_stats) = run_update(
        &mut cache,
        &new_ws,
        &sources(&sibling_new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_fresh(&new_ws, &sources(&sibling_new), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(
        cached
            .iter()
            .all(|f| f.lint != bone_lints::BACKEND_SIBLING_MISMATCH)
    );
    assert_eq!(sibling_stats.global_passes, 0);
    assert_eq!(sibling_stats.sibling_groups_revisited, 0);
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
        let old_ws = workspace(case.old);
        let new_ws = workspace(case.new);
        let (main_mid, main_owner) = function_owner(&new_ws, "main");
        let mut cache = IncrementalLintCache::default();
        run_update(
            &mut cache,
            &old_ws,
            &sources(case.old),
            &LintOptions::default(),
            &LintCacheUpdate::full(),
        );
        let (cached, stats) = run_update(
            &mut cache,
            &new_ws,
            &sources(case.new),
            &LintOptions::default(),
            &LintCacheUpdate::body_only([Ustr::from(FILE)], [(main_mid, main_owner)], []),
        );
        let fresh = lint_fresh(&new_ws, &sources(case.new), &LintOptions::default());

        assert_eq!(shapes(&cached), shapes(&fresh), "{name}");
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
    let old = [(FILE, "fn main() {}\n")];
    let new = [
        (FILE, "fn main() {}\n"),
        ("<incremental-cache-2>", "fn helper() {}\n"),
    ];
    let old_ws = workspace_many(&old);
    let new_ws = workspace_many(&new);
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &old_ws,
        &sources_many(&old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (cached, stats) = run_update(
        &mut cache,
        &new_ws,
        &sources_many(&new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([Ustr::from(FILE)], [], []),
    );
    let fresh = lint_fresh(&new_ws, &sources_many(&new), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert_eq!(stats.full_rebuilds, 1);
}

#[test]
fn suppression_removal_after_a_suppressed_rebuild_restores_the_finding() {
    let suppressed = "fn main() {\n    // rock: allow(unused_var)\n    let unused = 1\n}\n";
    let shown = "fn main() {\n    let unused = 1\n}\n";
    let suppressed_ws = workspace(suppressed);
    let shown_ws = workspace(shown);
    let file = Ustr::from(FILE);
    let removal = suppressed.find("// rock").expect("suppression");
    let mut cache = IncrementalLintCache::default();
    run_update(
        &mut cache,
        &suppressed_ws,
        &sources(suppressed),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (cached, stats) = run_update(
        &mut cache,
        &shown_ws,
        &sources(shown),
        &LintOptions::default(),
        &LintCacheUpdate::body_only(
            [file],
            [],
            [TextEditDelta {
                file,
                at: removal,
                byte_delta: -((suppressed.len() - shown.len()) as isize),
                line_delta: -1,
            }],
        ),
    );
    let fresh = lint_fresh(&shown_ws, &sources(shown), &LintOptions::default());
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert_eq!(stats.full_rebuilds, 0);
}

#[test]
fn cached_and_fresh_findings_share_one_order_after_a_body_update() {
    let src = "fn first(a: int) {\n    a == 1\n    let first_unused = 1\n}\nfn second(b: int) {\n    b == 2\n    let second_unused = 2\n}\nfn main() { first(1); second(2) }\n";
    let ws = workspace(src);
    let file = Ustr::from(FILE);
    let sources = sources(src);
    let options = LintOptions::default();
    let mut cache = IncrementalLintCache::default();
    let (rebuilt, _) = run_update(
        &mut cache,
        &ws,
        &sources,
        &options,
        &LintCacheUpdate::full(),
    );
    let (mid, owner) = function_owner(&ws, "second");
    let (updated, stats) = run_update(
        &mut cache,
        &ws,
        &sources,
        &options,
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_fresh(&ws, &sources, &options);

    assert_eq!(stats.full_rebuilds, 0);
    assert_eq!(fresh.len(), 4, "{:?}", shapes(&fresh));
    assert_eq!(shapes(&rebuilt), shapes(&fresh));
    assert_eq!(shapes(&updated), shapes(&fresh));
}

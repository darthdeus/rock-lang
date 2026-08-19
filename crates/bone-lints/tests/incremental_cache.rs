use bone_lints::{
    IncrementalLintCache, LintCacheUpdate, LintOptions, TextEditDelta, lint_workspace,
};
use bone_semantic::prelude::*;

const FILE: &str = "<incremental-cache>";

fn workspace(src: &str) -> Workspace {
    let ast = skull_parser::parse_module_clean(src, Ustr::from(FILE)).expect("parse");
    semantic_pass(&mut [ast]).expect("semantic pass")
}

fn workspace_many(srcs: &[(&str, &str)]) -> Workspace {
    let mut asts = srcs
        .iter()
        .map(|(file, src)| skull_parser::parse_module_clean(src, Ustr::from(*file)).expect("parse"))
        .collect::<Vec<_>>();
    semantic_pass(&mut asts).expect("semantic pass")
}

fn sources(src: &str) -> FxHashMap<Ustr, &str> {
    let mut out = FxHashMap::default();
    out.insert(Ustr::from(FILE), src);
    out
}

fn sources_many<'a>(srcs: &'a [(&'a str, &'a str)]) -> FxHashMap<Ustr, &'a str> {
    srcs.iter().map(|(file, src)| (Ustr::from(*file), *src)).collect()
}

fn shapes(findings: &[bone_lints::Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|finding| {
            let span = finding.error.labels.first().map(|label| label.span);
            format!("{}:{}:{:?}", finding.lint, finding.error.message, span)
        })
        .collect()
}

fn function_owner(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    let module = ws.modules.iter().find(|m| m.file == FILE).expect("module");
    let (fid, _) = module
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == name)
        .expect("function");
    (module.id, BodyOwner::Function(fid))
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
    cache.update(
        &old_ws,
        &FxHashSet::default(),
        true,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );

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
    let (cached, stats) = cache.update(
        &new_ws,
        &FxHashSet::default(),
        true,
        &new_sources,
        &LintOptions::default(),
        &update,
    );
    let fresh = lint_workspace(
        &new_ws,
        &FxHashSet::default(),
        true,
        &new_sources,
        &LintOptions::default(),
    );

    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(stats.body_updates > 0);
    assert_eq!(stats.full_rebuilds, 0);
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
    cache.update(
        &old_ws,
        &FxHashSet::default(),
        true,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (mid, owner) = function_owner(&new_ws, "main");
    let new_sources = sources(new);
    let (cached, stats) = cache.update(
        &new_ws,
        &FxHashSet::default(),
        true,
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
    let fresh = lint_workspace(
        &new_ws,
        &FxHashSet::default(),
        true,
        &new_sources,
        &LintOptions::default(),
    );

    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(stats.suppression_invalidations > 0);
    assert!(cached.iter().all(|finding| finding.lint != bone_lints::UNUSED_VAR));
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
    cache.update(
        &old_ws,
        &FxHashSet::default(),
        true,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (bad_findings, _) = cache.update(
        &bad_ws,
        &FxHashSet::default(),
        true,
        &sources(&bad),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    assert!(bad_findings.iter().any(|f| f.lint == bone_lints::TRANSMUTE_OF_NULLABLE));

    let fixed_ws = workspace(&fixed);
    let (fixed_mid, fixed_owner) = function_owner(&fixed_ws, "main");
    let (fixed_findings, stats) = cache.update(
        &fixed_ws,
        &FxHashSet::default(),
        true,
        &sources(&fixed),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(fixed_mid, fixed_owner)], []),
    );
    let fresh = lint_workspace(
        &fixed_ws,
        &FxHashSet::default(),
        true,
        &sources(&fixed),
        &LintOptions::default(),
    );
    assert_eq!(shapes(&fixed_findings), shapes(&fresh));
    assert!(fixed_findings.iter().all(|f| f.lint != bone_lints::TRANSMUTE_OF_NULLABLE));
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
    cache.update(
        &old_ws,
        &FxHashSet::default(),
        true,
        &old_sources,
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let new_sources = sources(new);
    let (cached, stats) = cache.update(
        &new_ws,
        &FxHashSet::default(),
        true,
        &new_sources,
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_workspace(
        &new_ws,
        &FxHashSet::default(),
        true,
        &new_sources,
        &LintOptions::default(),
    );
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(cached.iter().all(|f| f.lint != bone_lints::UNUSED_FN));
    assert!(stats.global_passes > 0);

    let sibling_old = "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\nfn main() {}\n";
    let sibling_new = sibling_old.replace("inline(js) fn probe(x: int)", "inline(js) fn probe(x: float)");
    let old_ws = workspace(sibling_old);
    let new_ws = workspace(&sibling_new);
    let (mid, owner) = function_owner(&new_ws, "main");
    let mut cache = IncrementalLintCache::default();
    cache.update(
        &old_ws,
        &FxHashSet::default(),
        true,
        &sources(sibling_old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (cached, _) = cache.update(
        &new_ws,
        &FxHashSet::default(),
        true,
        &sources(&sibling_new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([file], [(mid, owner)], []),
    );
    let fresh = lint_workspace(
        &new_ws,
        &FxHashSet::default(),
        true,
        &sources(&sibling_new),
        &LintOptions::default(),
    );
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert!(cached.iter().any(|f| f.lint == bone_lints::BACKEND_SIBLING_MISMATCH));
}

#[test]
fn file_set_change_takes_the_explicit_full_reconstruction_route() {
    let old = [(FILE, "fn main() {}\n")];
    let new = [(FILE, "fn main() {}\n"), ("<incremental-cache-2>", "fn helper() {}\n")];
    let old_ws = workspace_many(&old);
    let new_ws = workspace_many(&new);
    let mut cache = IncrementalLintCache::default();
    cache.update(
        &old_ws,
        &FxHashSet::default(),
        true,
        &sources_many(&old),
        &LintOptions::default(),
        &LintCacheUpdate::full(),
    );
    let (cached, stats) = cache.update(
        &new_ws,
        &FxHashSet::default(),
        true,
        &sources_many(&new),
        &LintOptions::default(),
        &LintCacheUpdate::body_only([Ustr::from(FILE)], [], []),
    );
    let fresh = lint_workspace(
        &new_ws,
        &FxHashSet::default(),
        true,
        &sources_many(&new),
        &LintOptions::default(),
    );
    assert_eq!(shapes(&cached), shapes(&fresh));
    assert_eq!(stats.full_rebuilds, 1);
}

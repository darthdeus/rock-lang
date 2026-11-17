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
/// `function`.
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
    run_update(cache, &ws, &sources, selection, &change)
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
    let (findings, _) = edit(&mut cache, added, Some("main"), None);
    assert!(findings.iter().any(|f| f.lint == UNUSED_VAR));

    let (findings, _) = edit(&mut cache, "fn main() { print(1) }\n", Some("main"), None);
    assert!(findings.iter().all(|f| f.lint != UNUSED_VAR));
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
        let at = suppressed.find("// rock").expect("suppression");
        let mut cache = seed(old);
        let (hidden, _) = edit(
            &mut cache,
            suppressed,
            None,
            Some(shift(at, old, suppressed, 1)),
        );
        assert!(hidden.is_empty());
        let (shown, _) = edit(
            &mut cache,
            old,
            Some("main"),
            Some(shift(at, suppressed, old, -1)),
        );
        assert_eq!(shown.iter().filter(|f| f.lint == lint).count(), 1);
    }
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
    let (findings, _) = body_edit(
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
}

#[test]
fn deprecated_findings_refresh_in_reinferred_consumers() {
    let main = "fn main() { old() }\n";
    let before = "fn old() {}\n";
    let after = "@deprecated(\"use fresh\")\nfn old() {}\n";
    let options = LintOptions::default();
    let mut cache = seeded_cache(&[("<dep-main>", main), ("<dep-callee>", before)], &options);
    let (findings, _) = body_edit(
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
fn cached_findings_match_fresh_after_body_edit_and_span_shift() {
    let old = "fn first() {\n    let first_unused = 1\n}\nfn second() {\n    let second_unused = 2\n}\nfn main() { first(); second() }\n";
    let new = "fn first() {\n    let first_unused = 1\n}\n// inserted before an unchanged finding\nfn second() {\n    let second_unused = 2\n}\nfn main() { first(); second() }\n";
    let mut cache = seed(old);
    let options = LintOptions::default();
    let insertion = old.find("fn second").expect("second function");
    let (ws, sources) = support::setup(&[(FILE, new)]);
    let change = LintCacheUpdate::body_only(
        [Ustr::from(FILE)],
        [function_owner(&ws, "first")],
        Some(shift(insertion, old, new, 1)),
    );
    let (updated, _) = run_update(&mut cache, &ws, &sources, &options, &change);
    assert_matches_fresh(&updated, &ws, &sources, &options);
    let second = updated
        .iter()
        .find(|finding| support::quoted(finding) == "second_unused")
        .expect("the unchanged finding survives the insertion");
    assert_eq!(
        second.error.labels[0].span.offset_range().0,
        new.find("second_unused").expect("source position")
    );
}

#[test]
fn findings_keep_source_order_after_a_body_edit() {
    let src = "fn first(a: int) {\n    a == 1\n    let first_unused = 1\n}\nfn second(b: int) {\n    b == 2\n    let second_unused = 2\n}\nfn main() { first(1); second(2) }\n";
    let mut cache = seed(src);
    let (findings, _) = edit(&mut cache, src, Some("second"), None);
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

#[test]
fn action_caller_edits_refresh_opt_in_findings() {
    let uncalled =
        "global ACT_USED: int = 1\nfn gameplay() { let value = 0 }\nfn main() { gameplay() }\n";
    let called = "global ACT_USED: int = 1\nfn gameplay() { let value = ACT_USED }\nfn main() { gameplay() }\n";
    let selection = action_selection();
    let mut cache = seeded_cache(&[(ACTION_FILE, uncalled)], &selection);
    for (src, expected) in [(called, 0), (uncalled, 1)] {
        let (cached, _) = body_edit(
            &mut cache,
            &[(ACTION_FILE, src)],
            &selection,
            &[ACTION_FILE],
            Some("gameplay"),
            None,
        );
        assert_eq!(cached.len(), expected);
        assert!(cached.iter().all(|f| f.error.message.contains("ACT_USED")));
    }
}

#[test]
fn sibling_suppression_edits_remove_stale_findings() {
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
    assert!(
        crate::lint(&[(FILE, old)], &selection, true)
            .iter()
            .any(|finding| finding.lint == bone_lints::BACKEND_SIBLING_MISMATCH)
    );
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

    let (fixed_findings, _) = edit(&mut cache, &good, Some("main"), None);
    assert!(
        fixed_findings
            .iter()
            .all(|f| f.lint != bone_lints::TRANSMUTE_OF_NULLABLE)
    );
}

/// Call graph edits update which functions are reachable from `main`.
#[test]
fn unused_fn_caller_edits_update_reachability() {
    struct Case {
        name: &'static str,
        old: &'static str,
        new: &'static str,
        dead: &'static [&'static str],
        live: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "adding the last caller",
            old: "fn helper() {}\nfn other_dead() {}\nfn main() {}\n",
            new: "fn helper() {}\nfn other_dead() {}\nfn main() { helper() }\n",
            dead: &["other_dead"],
            live: &["helper"],
        },
        Case {
            name: "removing the last caller",
            old: "fn helper() {}\nfn other_dead() {}\nfn main() { helper() }\n",
            new: "fn helper() {}\nfn other_dead() {}\nfn main() {}\n",
            dead: &["helper", "other_dead"],
            live: &[],
        },
        Case {
            name: "removing the root of a cycle",
            old: "fn a() { b() }\nfn b() { a() }\nfn main() { a() }\n",
            new: "fn a() { b() }\nfn b() { a() }\nfn main() {}\n",
            dead: &["a", "b"],
            live: &[],
        },
        Case {
            name: "adding a root to a cycle",
            old: "fn a() { b() }\nfn b() { a() }\nfn main() {}\n",
            new: "fn a() { b() }\nfn b() { a() }\nfn main() { a() }\n",
            dead: &[],
            live: &["a", "b"],
        },
        Case {
            name: "a generic callee resolves through its specialization",
            old: "fn identity<T>(value: T) -> T { value }\nfn main() {}\n",
            new: "fn identity<T>(value: T) -> T { value }\nfn main() { identity(1) }\n",
            dead: &[],
            live: &["identity"],
        },
        Case {
            name: "a UFCS call resolves to its method",
            old: "struct Mob { hp: int }\nfn Mob.damage(m, amount: int) { m.hp -= amount }\nfn main() {}\n",
            new: "struct Mob { hp: int }\nfn Mob.damage(m, amount: int) { m.hp -= amount }\nfn main() { Mob { hp: 1 }.damage(1) }\n",
            dead: &[],
            live: &["damage"],
        },
    ];
    for case in cases {
        let name = case.name;
        let mut cache = seed(case.old);
        let (cached, _) = edit(&mut cache, case.new, Some("main"), None);
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
    }
}

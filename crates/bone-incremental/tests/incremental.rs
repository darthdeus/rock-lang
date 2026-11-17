// Incremental == fresh oracle for `bone_semantic::Session` (see
// incremental compiler invariants): apply an edit script to a session, batch `semantic_pass` the final
// texts, and assert the two workspaces are equivalent. The comparator itself is
// validated batch-vs-batch below (equal programs compare equal, injected
// divergences are detected) so a green oracle run is evidence, not vacuous.
//
// Equivalence is text-anchored, not index-anchored: diagnostics compare as
// (file, offset, message) sets, and types compare via `query::type_at` at every
// byte offset of every file. This keeps the oracle valid even when the
// narrowing reuses bodies whose arena indices no longer match a fresh batch
// lowering.

use bone_incremental::session::{Session, UpdateResult, UpdateTier};
use bone_semantic::prelude::*;

use crate::common;
use common::{mid_for, type_profile};

fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from(file))
}

fn warm_prelude() -> skull_parser::Ast {
    let mut warm = parse_rock_with_file(WARM_PRELUDE, WARM_PRELUDE_PATH).expect("warm parses");
    warm.package = Some(common::package_tag(1, common::PACKAGE));
    warm
}

// ---------------------------------------------------------------------------
// Harness: parse + batch check
// ---------------------------------------------------------------------------

type Files = Vec<bone_incremental::session::SessionFile>;

use common::files;

/// True if `mid` is a warm stdlib module. With an opaque id, stdlib-ness is
/// read off the module's file, not the id.
fn is_stdlib_mid(ws: &Workspace, mid: ModuleId) -> bool {
    ws.modules
        .get(mid.index())
        .is_some_and(|m| rock_stdlib::is_stdlib_module_file(m.file.as_str()))
}

fn parse_files(files: &Files) -> Result<Vec<skull_parser::Ast>, CompilerError> {
    let mut modules = Vec::with_capacity(files.len());
    for f in files {
        let (path, text) = (&f.path, &f.text);
        let mut module = skull_parser::parse_module(text, Ustr::from(path.as_str()))?;
        // The oracle must see what the session sees: `Session::new` stamps
        // these same facts on every parse.
        module.package = f.package.clone();
        module.global_policy = f.global_policy;
        assert!(
            !module.items.is_empty(),
            "harness fixture {path} parsed to zero items; bone keys modules by \
             first-item span so an empty file would collide on ModuleId(\"\")"
        );
        modules.push(module);
    }
    Ok(modules)
}

/// Fresh whole-workspace check of warm ++ `files` — the oracle's ground
/// truth. `warm` are the same parsed modules the session under test was
/// built with, in the same (first) position. `aliases` is the host graph's
/// table for real-project sets, default (empty) for single-package fixtures.
fn batch_check(
    warm: &[skull_parser::Ast],
    files: &Files,
    aliases: &bone_core::modules::NsAliases,
) -> Result<Workspace, CompilerError> {
    let mut modules = warm.to_vec();
    modules.extend(parse_files(files)?);
    semantic_pass_with_aliases(&mut modules, aliases)
}

// ---------------------------------------------------------------------------
// Comparator
// ---------------------------------------------------------------------------

/// All body-level diagnostics as comparable, deterministically ordered keys.
fn diag_keys(ws: &Workspace) -> Vec<(String, usize, String)> {
    let mut keys: Vec<_> = ws
        .inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .map(|e| {
            (
                e.location.file.to_string(),
                e.location.offset,
                e.message.clone(),
            )
        })
        .collect();
    keys.sort();
    keys
}

/// Structural equivalence of two checked workspaces over the same file set.
/// Returns the first divergence as a human-readable description.
fn ws_equiv(
    left: &Workspace,
    right: &Workspace,
    files: &Files,
    stride: usize,
) -> Result<(), String> {
    let (ld, rd) = (diag_keys(left), diag_keys(right));
    if ld != rd {
        return Err(format!(
            "diagnostics diverge:\n  left:  {ld:?}\n  right: {rd:?}"
        ));
    }

    for f in files {
        let (path, text) = (&f.path, &f.text);
        let lp = type_profile(left, path, text, stride);
        let rp = type_profile(right, path, text, stride);
        for (i, (l, r)) in lp.iter().zip(rp.iter()).enumerate() {
            if l != r {
                let offset = i * stride;
                let line = text[..offset].matches('\n').count() + 1;
                return Err(format!(
                    "type diverges at {path}:{line} (offset {offset}):\n  left:  {l:?}\n  right: {r:?}"
                ));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Comparator self-tests (batch vs batch — no Session involved). These prove
// the comparator can actually see divergence; without them a green oracle
// could be green because the comparator is blind.
// ---------------------------------------------------------------------------

const CROSS_FILE_A: &str = "
struct Point {
    x: int,
    y: int,
}

fn make_point(x: int, y: int) -> Point {
    Point { x: x, y: y }
}
";

const CROSS_FILE_B: &str = "
fn double_x(p: Point) -> int {
    p.x * 2
}

fn origin_doubled() -> int {
    double_x(make_point(0, 0))
}
";

fn batch(files: &Files) -> Workspace {
    batch_check(&[], files, &bone_core::modules::NsAliases::default()).expect("batch check")
}

#[test]
fn comparator_sees_equal_and_divergent_programs() {
    let fs = files(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    ws_equiv(&batch(&fs), &batch(&fs), &fs, 1).expect("identical batch runs compare equal");

    // Same length and node positions; only the inferred type differs.
    let int_version = files(&[("a.rock", "fn f() -> int   { let v = 1   \n v }")]);
    let float_version = files(&[("a.rock", "fn f() -> float { let v = 1.0 \n v }")]);
    let err = ws_equiv(
        &batch(&int_version),
        &batch(&float_version),
        &int_version,
        1,
    )
    .expect_err("differing inferred types");
    assert!(err.contains("type diverges"), "{err}");

    let clean = files(&[("a.rock", "fn f() -> int { 1 }")]);
    let broken = files(&[("a.rock", "fn f() -> int { true }")]);
    let err =
        ws_equiv(&batch(&clean), &batch(&broken), &clean, 1).expect_err("differing diagnostics");
    assert!(err.contains("diagnostics diverge"), "{err}");
}

// ---------------------------------------------------------------------------
// Edit-script harness: every session mutation is immediately followed by a
// fresh batch check of the same texts and a full equivalence assertion.
// ---------------------------------------------------------------------------

/// Minimal warm module standing in for the stdlib: arithmetic desugars to
/// `__op_*` calls, so stdlib-free fixtures need these to type-check. Living
/// in the WARM set means every oracle test also exercises warm-module
/// retention across updates; the batch side gets the same text as a plain
/// first file, which checks to an equivalent workspace.
const WARM_PRELUDE_PATH: &str = "<warm-prelude>";
const WARM_PRELUDE: &str = "
fn __op_add(a: int, b: int) -> int { 0 }
fn __op_sub(a: int, b: int) -> int { 0 }
fn __op_mul(a: int, b: int) -> int { 0 }
";

struct Harness {
    session: Session,
    /// The session's warm modules, replayed as the first batch modules.
    warm: Vec<skull_parser::Ast>,
    /// Ground-truth user texts in the session's tracking order.
    texts: Files,
    /// Offset stride for the per-offset type comparison.
    stride: usize,
    /// The host graph's alias tables (default/empty for this single package).
    aliases: bone_core::modules::NsAliases,
    /// The language server's lint cache, fed the change each update implies;
    /// it must agree with a fresh lint pass after every step.
    lints: bone_lints::IncrementalLintCache,
}

impl Harness {
    fn new(initial: &[(&str, &str)]) -> Harness {
        let warm = warm_prelude();
        Harness::with_warm(vec![warm], files(initial), 1)
    }

    fn with_warm(warm: Vec<skull_parser::Ast>, texts: Files, stride: usize) -> Harness {
        Harness::with_warm_aliases(
            warm,
            texts,
            stride,
            bone_core::modules::NsAliases::default(),
        )
    }

    /// [`with_warm`] for real-project sets, whose v1 `use` paths resolve
    /// through the host graph's alias tables.
    fn with_warm_aliases(
        warm: Vec<skull_parser::Ast>,
        texts: Files,
        stride: usize,
        aliases: bone_core::modules::NsAliases,
    ) -> Harness {
        let session = Session::new(warm.clone(), texts.clone(), &aliases).expect("session init");
        let mut h = Harness {
            session,
            warm,
            texts,
            stride,
            aliases,
            lints: bone_lints::IncrementalLintCache::default(),
        };
        h.assert_matches_batch();
        h.assert_lints_match(&bone_lints::LintCacheUpdate::full());
        h
    }

    fn update(&mut self, path: &str, text: &str) -> UpdateResult {
        if !self.session.is_tracked(path) {
            self.session
                .declare_file_facts(
                    path,
                    Some(common::package_tag(1, common::PACKAGE)),
                    skull_parser::GlobalPolicy::Any,
                )
                .expect("new fixture files use the shared v1 package");
        }
        let result = self
            .session
            .update_file(path, text)
            .unwrap_or_else(|e| panic!("update_file({path}) failed: {}", e.message));
        match self.texts.iter_mut().find(|f| f.path == path) {
            Some(f) => f.text = text.to_string(),
            None => self.texts.push(common::v1(path, text)),
        }
        self.assert_matches_batch();
        let change = bone_query::lint_cache_update(self.session.workspace(), path, &result);
        self.assert_lints_match(&change);
        result
    }

    fn remove(&mut self, path: &str) -> UpdateResult {
        let result = self
            .session
            .remove_file(path)
            .unwrap_or_else(|e| panic!("remove_file({path}) failed: {}", e.message));
        self.texts.retain(|f| f.path != path);
        self.assert_matches_batch();
        self.lints.invalidate_file(Ustr::from(path));
        self.assert_lints_match(&bone_lints::LintCacheUpdate::body_only([], [], []));
        result
    }

    /// Apply `change` to the lint cache and compare its findings with a fresh
    /// whole-workspace lint pass over the same session workspace.
    fn assert_lints_match(&mut self, change: &bone_lints::LintCacheUpdate) {
        let sources: FxHashMap<Ustr, &str> = self
            .texts
            .iter()
            .map(|f| (Ustr::from(f.path.as_str()), f.text.as_str()))
            .collect();
        let ws = self.session.workspace();
        let warm = &self.session.warm_mids;
        let options = bone_lints::LintOptions::default();
        let (cached, _) = self
            .lints
            .update(ws, warm, true, &sources, &options, change);
        let fresh = bone_lints::lint_workspace(ws, warm, true, &sources, &options);
        let render = |findings: &[bone_lints::Finding]| {
            let mut rows: Vec<String> = findings.iter().map(|f| format!("{f:?}")).collect();
            rows.sort();
            rows
        };
        assert_eq!(
            render(&cached),
            render(&fresh),
            "incremental lint findings != fresh after {change:?}"
        );
    }

    fn assert_matches_batch(&self) {
        let batch = batch_check(&self.warm, &self.texts, &self.aliases)
            .expect("batch check of session texts");
        ws_equiv(self.session.workspace(), &batch, &self.texts, self.stride)
            .unwrap_or_else(|d| panic!("incremental != fresh: {d}"));
    }

    /// Distinct user modules named in `reinferred`.
    fn reinferred_mids(result: &UpdateResult) -> FxHashSet<ModuleId> {
        result.reinferred.iter().map(|(mid, _)| *mid).collect()
    }

    /// Names of the distinct functions among `ids`, sorted.
    fn function_names(&self, ids: &[(ModuleId, bone_semantic::hir::BodyOwner)]) -> Vec<String> {
        let ws = self.session.workspace();
        let mut names: Vec<String> = ids
            .iter()
            .filter_map(|(mid, owner)| match owner {
                bone_semantic::hir::BodyOwner::Function(fid) => Some(
                    ws.module(*mid).functions[*fid]
                        .name
                        .as_str()
                        .rsplit("::")
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    #[track_caller]
    fn assert_diagnostic(&self, needle: &str) {
        let diags: Vec<_> = self
            .session
            .diagnostics()
            .iter()
            .map(|e| format!("{}: {}", e.location.file, e.message))
            .collect();
        assert!(
            diags.iter().any(|d| d.contains(needle)),
            "no diagnostic contains {needle:?}: {diags:#?}"
        );
    }

    /// The dense `ModuleId` for a tracked file, resolved via the session's
    /// interner (the id is opaque now, so it can't be fabricated from the path).
    fn mid(&self, path: &str) -> ModuleId {
        mid_for(self.session.workspace(), path)
    }

    #[track_caller]
    fn assert_clean(&self) {
        let diags: Vec<_> = self
            .session
            .diagnostics()
            .iter()
            .map(|e| format!("{}:{} {}", e.location.file, e.location.offset, e.message))
            .collect();
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:#?}");
    }
}

// ---------------------------------------------------------------------------
// Oracle tests: session == batch after every step of each script.
// ---------------------------------------------------------------------------

/// A macro's recorded def-span (used by ctags) must stay line-accurate across a
/// fast body edit that shifts it. Editing a function body ABOVE the macro and
/// adding a line takes the BodyOnly fast path, which span-shifts the kept items
/// in place rather than reparsing -- the macro def-line must move with them.
#[test]
fn macro_def_span_shifts_on_fast_body_edit() {
    // macro `m` is on line 5 (0-based 4): fn(1-3), blank(4), macro(5).
    let v1 = "fn helper() -> int {\n  1\n}\n\nmacro m(x: expr) -> int {\n  x\n}\n";
    let mut h = Harness::new(&[("m.rock", v1)]);
    let before = &h.session.files["m.rock"].macros;
    assert_eq!(before.len(), 1, "one macro recorded");
    assert_eq!(before[0].name.as_str(), "m");
    assert_eq!(
        before[0].span.line_range().0,
        4,
        "macro starts on line 5 (0-based 4)"
    );

    // Grow `helper`'s body by one line; `m` slides from line 5 to line 6.
    let v2 = "fn helper() -> int {\n  let a = 2\n  a\n}\n\nmacro m(x: expr) -> int {\n  x\n}\n";
    let result = h.update("m.rock", v2);
    assert_eq!(
        result.tier,
        UpdateTier::BodyOnly,
        "a localized body edit is BodyOnly"
    );

    let after = &h.session.files["m.rock"].macros;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].span.line_range().0,
        5,
        "macro def-line must follow the +1 line shift (was 4, now 5)"
    );
}

/// A `co fn` body edit stays a coroutine and is reported in `reinferred`
/// (the overlay re-emits exactly those bodies). The parser consumes `co`
/// before `parse_fn`, so the function span starts at `fn` while the item span
/// starts at `co`: the fast-path reparse sees a plain `fn`, and a body locator
/// keyed on the item offset misses the body unless the footprint covers `co`.
#[test]
fn coroutine_body_edit_stays_a_coroutine_and_is_reported() {
    let v = |n: u32| {
        format!(
            "co fn counter() -> Coroutine<int> {{\n  yield {n}\n}}\nfn main() {{\n  let c = counter()\n}}\n"
        )
    };
    let mut h = Harness::new(&[("c.rock", &v(1))]);
    h.assert_clean();
    let result = h.update("c.rock", &v(10));
    assert_eq!(result.tier, UpdateTier::BodyOnly);
    assert_eq!(result.reinferred.len(), 1);
    assert_eq!(h.function_names(&result.reinferred), ["counter"]);
    h.assert_clean();
    let ws = h.session.workspace();
    let (fmid, fid) = ws
        .module(h.mid("c.rock"))
        .functions_by_name
        .iter()
        .find(|(name, _)| name.as_str().rsplit("::").next() == Some("counter"))
        .expect("counter function")
        .1[0];
    assert!(ws.module(fmid).functions[fid].is_coroutine);
}

/// Editing the BODY of a DECORATED function must report that body in
/// `reinferred`, same as a non-decorated edit (`oracle_body_edit`). Regression
/// guard for the body-locator fix (`lower.rs`: the `item_index` interval covers
/// the whole item footprint, decorators included, so `rebuild_module`'s
/// `body_at(item.offset)` resolves the decorated body instead of missing the
/// gap left by `@deco`).
///
/// Why it matters: skull's hot-reload overlay re-emits only what `reinferred`
/// reports; if a decorated `@update`/`@draw`/`@init` system body were dropped,
/// the overlay would carry the rebind but not the new body and the running game
/// would keep the stale code. Exercised downstream in `rock-cli`
/// `overlay_runtime_tests::sim_overlay_rebind_runs_new_system_body`.
#[test]
fn decorated_body_edit_reinfers_the_body() {
    let v0 = "global G: int = 0\ndeco init upd { }\n@upd\nfn tick() { G = 1 }";
    let v1 = "global G: int = 0\ndeco init upd { }\n@upd\nfn tick() { G = 2 }";
    let mut h = Harness::new(&[("m.rock", v0)]);
    let result = h.update("m.rock", v1);
    assert_eq!(result.tier, UpdateTier::BodyOnly);
    assert_eq!(h.function_names(&result.reinferred), ["tick"]);
}

/// Generations are keyed by function identity, not name: editing `foo(int)`
/// twice drives its generation to 2 while the `foo(float)` overload stays at 0.
#[test]
fn overloaded_body_edit_bumps_only_the_edited_overload() {
    let gens_in_order = |h: &Harness| -> Vec<u64> {
        let mid = h.mid("m.rock");
        let m = h.session.workspace().module(mid);
        m.functions
            .iter()
            .map(|(fid, _)| {
                m.bodies
                    .get(&bone_semantic::hir::BodyOwner::Function(fid))
                    .map(|b| b.generation)
                    .unwrap_or(0)
            })
            .collect()
    };
    let prog =
        |c: i64| format!("fn foo(x: int) -> int {{ {c} }}\nfn foo(x: float) -> float {{ 0.0 }}\n");

    let mut h = Harness::new(&[("m.rock", &prog(1))]);
    assert_eq!(
        gens_in_order(&h),
        vec![0, 0],
        "fresh module starts at gen 0"
    );
    // Edit only foo(int)'s body, twice.
    let r1 = h.update("m.rock", &prog(2));
    assert_eq!(r1.tier, UpdateTier::BodyOnly);
    let r2 = h.update("m.rock", &prog(3));
    assert_eq!(r2.tier, UpdateTier::BodyOnly);
    assert_eq!(
        gens_in_order(&h),
        vec![2, 0],
        "foo(int) must bump per edit (->2); foo(float) preserved (0) -- name-keying \
         would stick foo(int) at 1 by colliding with foo(float)"
    );
}

// n functions; f0's body carries constant `c0` so editing it is a real change.
fn cook_premise_module(n: usize, c0: i64) -> String {
    let mut s = String::new();
    for i in 0..n {
        let k = if i == 0 { c0 } else { i as i64 };
        s.push_str(&format!(
            "fn f{i}(x: int) -> int {{ let a = x + {k}\n let b = a * 2\n let c = b - {i}\n return c }}\n"
        ));
    }
    s
}

/// A body edit re-infers exactly one body regardless of module size, read from
/// `reinferred_body_count` (the number of `check_function` calls).
#[test]
fn body_edit_reinfers_only_the_changed_body() {
    use std::sync::atomic::Ordering;
    for n in [20usize, 400] {
        let mut h = Harness::new(&[("m.rock", &cook_premise_module(n, 0))]);
        // A few alternating real edits to f0's constant; read the LAST update's
        // count (each edit differs from the previous, so each is a BodyOnly edit).
        let mut count = 0;
        for i in 0..6 {
            let c0 = 1 + (i % 2) as i64;
            let r = h.update("m.rock", &cook_premise_module(n, c0));
            assert_eq!(
                r.tier,
                UpdateTier::BodyOnly,
                "f0 const edit must be BodyOnly (n={n})"
            );
            count = h.session.ws.reinferred_body_count.load(Ordering::Relaxed);
        }
        assert_eq!(
            count, 1,
            "a body edit must re-infer EXACTLY the changed body, not scale with \
             module size (n={n}): reuse cached BodyInference where \
             body_generation == body.generation"
        );
    }
}

#[test]
fn oracle_signature_edit_breaks_then_fixes_cross_file_caller() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);

    // make_point loses a param: b.rock's two-arg call site goes stale.
    // (An int->float param change would NOT break the caller - int literals
    // coerce to float.)
    let changed = CROSS_FILE_A
        .replace("x: int, y: int", "x: int")
        .replace("y: y", "y: x");
    h.update("a.rock", &changed);
    assert!(
        !h.session.diagnostics().is_empty(),
        "caller in b.rock must error after the signature change"
    );

    // Fix the caller; everything recovers.
    h.update(
        "b.rock",
        &CROSS_FILE_B.replace("make_point(0, 0)", "make_point(0)"),
    );
    h.assert_clean();
}

/// REGRESSION (a 0-byte source file): an item-less user file
/// interned BEFORE another user file must not corrupt module-id alignment.
///
/// `Session::new` interns every file, but `combined_unexpanded` omits item-less
/// modules (`!items.is_empty()`). The resulting module-id gap made
/// `insert_module` -- which `push`es when `idx >= len`, unlike the resize-based
/// `set_inference` -- misplace every module after the hole. `module(mid)` then
/// returned the wrong `Module` (or went OOB), so inference / overload resolution
/// indexed the wrong `functions` arena and panicked (`score_candidate`, the
/// `ws.module(*id)` in pass 4). The batch `semantic_pass` path is immune: it
/// interns exactly the modules it inserts, so no gap forms.
///
/// Driven through `Session` directly -- the `Harness` batch oracle (`parse_files`)
/// rejects empty files, so it can't express this. `later.rock` lands after the
/// hole and its self-call drives resolution through the misaligned id.
#[test]
fn itemless_file_before_others_keeps_module_ids_aligned() {
    let warm = warm_prelude();
    let session = Session::new(
        vec![warm],
        files(&[
            ("empty.rock", ""),
            (
                "later.rock",
                "fn foo() -> int { 1 }\nfn use_foo() -> int { foo() }",
            ),
        ]),
        &bone_core::modules::NsAliases::default(),
    )
    .expect("session init must not crash on an item-less file");

    let diags: Vec<_> = session
        .diagnostics()
        .iter()
        .map(|e| format!("{}:{} {}", e.location.file, e.location.offset, e.message))
        .collect();
    assert!(
        diags.is_empty(),
        "post-gap module mis-resolved (id misalignment): {diags:#?}"
    );
}

#[test]
fn oracle_add_then_remove_file() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A)]);

    h.update(
        "c.rock",
        "
fn sum_xy(p: Point) -> int {
    p.x + p.y
}
",
    );
    h.assert_clean();
    assert!(h.session.is_tracked("c.rock"));

    let result = h.remove("c.rock");
    assert_eq!(result.tier, UpdateTier::FullUser);
    assert!(!h.session.is_tracked("c.rock"));

    // Removing an untracked path is a quiet no-op.
    let result = h.session.remove_file("c.rock").expect("untracked remove");
    assert_eq!(result.tier, UpdateTier::NoChange);
}

#[test]
fn oracle_delete_defining_file_breaks_body_dependent_then_restore_recovers() {
    // d.rock depends on a.rock only from a BODY (its signature is
    // stdlib-free), so deleting a.rock degrades to a per-body error and the
    // session keeps running.
    let body_dependent = "
fn count_points() -> int {
    let p = make_point(1, 2)
    p.x
}
";
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("d.rock", body_dependent)]);
    h.assert_clean();

    h.remove("a.rock");
    assert!(
        !h.session.diagnostics().is_empty(),
        "d.rock calls make_point from its body and must error once a.rock is gone"
    );

    h.update("a.rock", CROSS_FILE_A);
    h.assert_clean();
}

#[test]
fn oracle_for_iter_protocol_invalidates_on_next_edit() {
    // `for x in <user type>` resolves the iter/next protocol (bone records the
    // fns in `for_iter_protocol`). Removing `next` must re-typecheck the loop and
    // surface the error; restoring recovers. `assert_matches_batch` (inside
    // `update`) also proves the side-table is recomputed identically to a fresh
    // batch check after each edit.
    let with_next = "\
struct R { hi: int }
struct RIt { cur: int, hi: int }
fn iter(r: R) -> RIt { RIt { cur: 0, hi: r.hi } }
fn next(it: RIt) -> (bool, int) {
    if it.cur < it.hi { let v = it.cur; it.cur = it.cur + 1; (true, v) } else { (false, 0) }
}
fn total() -> int { let s = 0; for x in (R { hi: 3 }) { s = s + x } s }
";
    let mut h = Harness::new(&[("it.rock", with_next)]);
    h.assert_clean();

    // Drop `next` -> the loop can't resolve the protocol.
    let start = with_next.find("fn next").unwrap();
    let end = with_next.find("fn total").unwrap();
    let without_next = format!("{}{}", &with_next[..start], &with_next[end..]);
    h.update("it.rock", &without_next);
    assert!(
        !h.session.diagnostics().is_empty(),
        "removing `next` must break the for-loop over R"
    );

    // Restore -> recovers.
    h.update("it.rock", with_next);
    h.assert_clean();
}

#[test]
fn delete_of_signature_dependency_surfaces_dependent_error() {
    // b.rock names `Point` in a SIGNATURE. With total lowering, removing a.rock
    // (Point's definition) is a normal remove via the full rebuild: b.rock's
    // now-unresolved signature recovers to `Error` and the diagnostic surfaces,
    // instead of fail-fasting + reverting (the plan-C behavior this replaces).
    // `remove`/`update` assert incremental == fresh batch at every step.
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    h.assert_clean();

    h.remove("a.rock");
    h.assert_diagnostic("Point");
    assert!(
        !h.session.is_tracked("a.rock"),
        "the remove commits (no revert)"
    );

    h.update("a.rock", CROSS_FILE_A);
    h.assert_clean();
}

#[test]
fn oracle_introduce_and_fix_type_error() {
    let mut h = Harness::new(&[("a.rock", "fn f() -> int { 1 }")]);
    h.assert_clean();

    h.update("a.rock", "fn f() -> int { true }");
    assert_eq!(h.session.diagnostics().len(), 1);

    h.update("a.rock", "fn f() -> int { 2 }");
    h.assert_clean();
}

#[test]
fn oracle_noop_update_short_circuits() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    let result = h.update("a.rock", CROSS_FILE_A);
    assert_eq!(result.tier, UpdateTier::NoChange);
    assert!(result.reinferred.is_empty());
}

#[test]
fn update_with_unlowerable_signature_recovers_and_surfaces() {
    // Parses fine, fails interface lowering (unknown signature type). With total
    // lowering the param type recovers to `Error`, the edit COMMITS (no revert),
    // and the diagnostic surfaces. The edited module is re-lowered, so the
    // oracle (incremental == fresh) holds; fixing it converges to clean.
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    h.update("b.rock", "fn f(p: Nonexistent) -> int { 1 }");
    h.assert_diagnostic("Nonexistent");

    h.update("b.rock", CROSS_FILE_B);
    h.assert_clean();
}

#[test]
fn oracle_broken_type_definer_cascades_to_dependent() {
    // Editing a.rock (Point's definition) to a broken state must propagate to
    // b.rock, which names `Point` in a SIGNATURE: the incremental session must
    // match a fresh batch build (the oracle), which means b.rock is re-LOWERED,
    // not just re-inferred. This is the interface-change cascade; a narrow
    // rebuild_interface would re-infer b.rock against a stale signature and the
    // oracle would catch the divergence.
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    h.assert_clean();

    // Drop Point entirely from a.rock (its only items are Point + make_point).
    h.update("a.rock", "fn unrelated() -> int { 1 }");
    h.assert_diagnostic("Point");

    h.update("a.rock", CROSS_FILE_A);
    h.assert_clean();
}

#[test]
fn oracle_transitive_interface_cascade() {
    // a defines T; b wraps it in a struct field (interface-position ref) AND a
    // body reads through it; c reads b's Wrap in a body. Removing T must ripple
    // a -> b (re-lower: Wrap.inner T->Error) -> c (re-infer: the body type it
    // reads shifts). The oracle (asserted at every step) is the judge -- the
    // expression types must match a fresh batch build across all three files.
    let a = "value T { v: int }";
    let b = "struct Wrap { inner: T }\nfn unwrap(w: Wrap) -> int { w.inner.v }";
    let c = "fn use_wrap(w: Wrap) -> int { unwrap(w) }";
    let mut h = Harness::new(&[("a.rock", a), ("b.rock", b), ("c.rock", c)]);
    h.assert_clean();

    h.update("a.rock", "fn nothing() -> int { 0 }"); // drop T
    assert!(
        !h.session.diagnostics().is_empty(),
        "removing T must surface an error somewhere downstream"
    );

    h.update("a.rock", a); // restore -- must converge clean
    h.assert_clean();
}

#[test]
fn shape_change_relowers_only_the_edited_module() {
    // Adding a field keeps the struct's NAME, so dependents' signatures still
    // resolve it -- they need only re-inference, not re-lowering. The cascade must
    // re-lower ONLY the edited module; regressing to the conservative closure
    // would re-lower every interface dependent (a shared type touched 22
    // modules vs 1). Deterministic guard via `relowered`.
    let a = "struct P { x: int }";
    let b = "fn read(p: P) -> int { p.x }"; // P in signature (interface dep) AND body
    let mut h = Harness::new(&[("a.rock", a), ("b.rock", b)]);
    h.assert_clean();

    let r = h.update("a.rock", "struct P { x: int\n y: int = 0 }"); // add a defaulted field
    let relowered_mods: FxHashSet<ModuleId> = r.relowered.iter().map(|(m, _)| *m).collect();
    assert_eq!(
        relowered_mods.len(),
        1,
        "a field add must re-lower only the edited module, not its dependents; got {relowered_mods:?}"
    );
    h.assert_clean(); // oracle held in h.update; defaulted field keeps b valid
}

#[test]
fn oracle_dependent_body_type_ref_recovers_not_reverts() {
    // A re-lowered dependent runs FULL body lowering. A body-position type ref to
    // the removed type (`let x: Foo`) hits the fail-fast `TyPos::Body` arm -- but
    // body recovery must catch it (the function keeps its signature, the body
    // error is recorded), so `lower_modules` returns Ok and the edit COMMITS. If
    // it instead `?`-aborted, rebuild_or_revert would revert and the oracle (which
    // expects the broken, committed state) would diverge.
    let a = "struct Foo { v: int }";
    let b = "fn sig(f: Foo) -> int { let x: Foo = f\n x.v }"; // Foo in BOTH signature and body
    let mut h = Harness::new(&[("a.rock", a), ("b.rock", b)]);
    h.assert_clean();

    h.update("a.rock", "fn nothing() -> int { 0 }"); // drop Foo
    assert!(
        !h.session.diagnostics().is_empty(),
        "removing Foo must surface b's dangling references"
    );

    h.update("a.rock", a);
    h.assert_clean();
}

#[test]
fn oracle_type_rename_cascades_both_directions() {
    // Renaming a type breaks references to the OLD name and (re)resolves the NEW
    // one. The oracle validates the whole workspace at each keystroke.
    let a = "struct Point { x: int }";
    let b = "fn px(p: Point) -> int { p.x }";
    let mut h = Harness::new(&[("a.rock", a), ("b.rock", b)]);
    h.assert_clean();

    h.update("a.rock", "struct Pointe { x: int }"); // rename Point -> Pointe
    h.assert_diagnostic("Point");

    h.update("b.rock", "fn px(p: Pointe) -> int { p.x }"); // fix the reference
    h.assert_clean();
}

/// A body edit of a macro user takes `BodyOnly` and re-cooks just that file
/// against the cached registry; an edit of the macro definition rebuilds the
/// registry, and the user sees the new expansion.
#[test]
fn oracle_add_and_remove_macro_definition() {
    let plain = "fn f() -> int { 1 }";
    let defined = "macro wrap(e: expr) { e }\nfn f() -> int { 1 }";
    let called = "macro wrap(e: expr) { e }\nfn f() -> int { wrap(1) }";
    let mut h = Harness::new(&[("m.rock", plain)]);
    h.update("m.rock", defined);
    h.assert_clean();
    h.update("m.rock", called);
    h.assert_clean();
    h.update("m.rock", "fn f() -> int { wrap(1) }");
    h.assert_diagnostic("wrap");
    h.update("m.rock", called);
    h.assert_clean();
}

#[test]
fn oracle_macro_user_body_edit_and_macro_def_edit() {
    let macro_def = |body: &str| {
        format!("macro twice(e: expr) {{\n    {body}\n}}\n\nfn base() -> int {{ 3 }}\n")
    };
    let mut h = Harness::new(&[
        ("m.rock", &macro_def("(e) + (e)")),
        ("use.rock", "fn quad() -> int { twice(base()) }"),
    ]);
    h.assert_clean();

    let r = h.update("use.rock", "fn quad() -> int { twice(base()) + 1 }");
    assert_eq!(r.tier, UpdateTier::BodyOnly);
    h.assert_clean();

    h.update("m.rock", &macro_def("(e) + (e) + (e)"));
    h.assert_clean();
}

/// Registry freshness across the cache boundary: after a macro-DEF change
/// rebuilds the registry (`rebuild_users`), a later BODY edit of the user must
/// cook against the NEW registry. Here the new macro produces a `string` while
/// the user still annotates `-> int`, so a stale cached registry (old identity
/// macro) would let the body edit type-check clean -- the fresh batch rejects
/// it, and the Harness's incremental==fresh assertion catches the divergence.
#[test]
fn oracle_macro_def_retype_then_user_body_edit_sees_new_registry() {
    let mut h = Harness::new(&[
        ("m.rock", "macro wrap(e: expr) {\n    e\n}\n"),
        ("use.rock", "fn f() -> int { wrap(5) }"),
    ]);
    h.assert_clean();

    // Def edit: wrap now yields a string. Routes to rebuild_users (registry
    // rebuilt); use.rock's `wrap(5)` is now string vs `-> int` -> error.
    h.update("m.rock", "macro wrap(e: expr) {\n    \"{e}\"\n}\n");

    // Body edit of the user: must cook against the NEW (string) registry. A
    // stale identity registry would make this pass; the batch comparator and
    // the persisting type error pin the freshness.
    h.update("use.rock", "fn f() -> int { wrap(5) + 0 }");
}

#[test]
fn oracle_globals_survive_repeated_updates() {
    // Regression: globals_by_name maps name -> TyId whose module is the
    // TYPE's home (builtin int for `global G: int`), not the defining module,
    // so a purge keyed on the ty's module left stale user globals behind and
    // every rebuild after the first died with "duplicate global". Found by
    // hand-running `rock watch example`.
    let with_global = "
global GRID_W: int = 8

fn area() -> int {
    GRID_W * GRID_W
}
";
    let mut h = Harness::new(&[("g.rock", with_global)]);
    h.assert_clean();

    // Any second rebuild re-registers the global; with the purge bug this
    // failed before reaching inference.
    h.update(
        "g.rock",
        &with_global.replace("GRID_W * GRID_W", "GRID_W * GRID_W * 1"),
    );
    h.assert_clean();

    // And a third, through the OTHER rebuild path (file add).
    h.update("h.rock", "fn use_area() -> int { area() }");
    h.assert_clean();
}

#[test]
fn session_body_update_metadata_separates_semantics_from_span_relocation() {
    let mut h = Harness::new(&[(
        "meta.rock",
        "fn first() -> int {\n    1\n}\n\nfn second() -> int {\n    2\n}\n",
    )]);

    let semantic = h.update(
        "meta.rock",
        "fn first() -> int {\n    1\n}\n\nfn second() -> int {\n    222\n}\n",
    );
    assert_eq!(semantic.tier, UpdateTier::BodyOnly);
    assert!(!semantic.body_update.span_only);
    assert_eq!(semantic.body_update.semantic_bodies, semantic.reinferred);
    let edit = semantic.body_update.edit.expect("body edit metadata");
    assert_eq!(edit.file, Ustr::from("meta.rock"));
    assert!(edit.byte_delta > 0, "the fixture must move later spans");
    assert!(edit.line_delta == 0);

    let span_only = h.update(
        "meta.rock",
        "fn first() -> int {\n    1\n}\n\n// moved without a body change\n\nfn second() -> int {\n    222\n}\n",
    );
    assert_eq!(span_only.tier, UpdateTier::BodyOnly);
    assert!(span_only.body_update.span_only);
    assert!(span_only.body_update.semantic_bodies.is_empty());
    assert!(span_only.reinferred.is_empty());
    let edit = span_only.body_update.edit.expect("layout edit metadata");
    assert!(edit.byte_delta > 0);
    assert!(edit.line_delta > 0);
}

#[test]
fn session_interface_update_does_not_publish_body_metadata() {
    let mut h = Harness::new(&[("meta-interface.rock", "fn value() -> int {\n    1\n}\n")]);
    let result = h.update(
        "meta-interface.rock",
        "fn value(x: int) -> int {\n    x\n}\n",
    );
    assert_ne!(result.tier, UpdateTier::BodyOnly);
    assert_eq!(result.body_update, Default::default());
}

// ---------------------------------------------------------------------------
// Per-body eviction hazard fixtures: green under the current module rebuild,
// these become the real guards once single-body eviction lands. Each targets
// one failure mode of partial re-lowering.
// ---------------------------------------------------------------------------

#[test]
fn per_body_hazard_edit_above_shifts_sibling_spans() {
    let source = |first, second| {
        format!(
            "fn first() -> int {{\n    {first}\n}}\n\nfn second() -> int {{\n    {second}\n}}\n"
        )
    };
    let mut h = Harness::new(&[("s.rock", &source("1", "first()"))]);
    h.assert_clean();
    for (first, second, errors) in [
        ("100000", "first()", 0),
        ("1", "true", 1),
        ("123456789", "true", 1),
    ] {
        h.update("s.rock", &source(first, second));
        assert_eq!(h.session.diagnostics().len(), errors);
    }
}

#[test]
fn body_edits_preserve_sibling_inference() {
    let cases = [
        // Generic and inferred VarIds share a module-wide counter.
        (
            "generic function beside a hole annotation",
            "fn ident<T>(x: T) -> T {\n    x\n}\n\nfn holes() -> int {\n    let v: _ = 41\n    v\n}\n\nfn uses_ident() -> int {\n    ident(7)\n}\n",
            "41",
            "42",
        ),
        (
            "global initializer and struct defaults",
            "global BASE: int = 10\n\nstruct Conf {\n    scale: int = 3,\n}\n\nfn use_conf(c: Conf) -> int {\n    c.scale + BASE\n}\n",
            "c.scale + BASE",
            "c.scale + BASE + BASE",
        ),
    ];
    for (case, initial, from, to) in cases {
        let mut h = Harness::new(&[("m.rock", initial)]);
        h.assert_clean();
        let text = initial.replace(from, to);
        h.update("m.rock", &text);
        assert!(h.session.diagnostics().is_empty(), "{case}");
    }
}

/// `type_at` at `offset` in `path` as `(rendered type, span.offset_range())`.
fn type_and_span_at(h: &Harness, path: &str, offset: usize) -> Option<(String, (usize, usize))> {
    let ws = h.session.workspace();
    let mid = mid_for(ws, path);
    let inf = ws.inference(mid)?;
    let ta = bone_semantic::query::type_at(ws, mid, inf, offset)?;
    Some((ws.render(ta.body, ta.ty), ta.span.offset_range()))
}

/// A length-changing edit to an earlier body shifts every later node. The kept
/// bodies' stored spans must move by exactly the delta: a span off by the
/// delta can still land inside a node of the same type, which the per-offset
/// type profile cannot see, so this reads the span itself.
#[test]
fn body_edit_above_keeps_later_node_spans_exact() {
    let v0 = "\
fn edited() -> int { 1 }
fn floaty() -> float {
    let m = 3.14
    m
}
fn booly() -> bool {
    let t = true
    t
}
";
    let mut h = Harness::new(&[("m.rock", v0)]);
    h.assert_clean();

    // Anchor the later nodes in the ORIGINAL text; assert they resolve before
    // the edit so the post-edit assertions are measuring a real shift.
    let flo0 = v0.find("3.14").expect("float literal present");
    assert_eq!(
        type_and_span_at(&h, "m.rock", flo0),
        Some(("float".to_string(), (flo0, flo0 + "3.14".len()))),
        "float literal must resolve to its own span before the edit"
    );

    // Length-changing edit to the FIRST body: `{ 1 }` -> `{ 123456789 }`
    // (+8 bytes), shifting floaty and booly down by 8.
    let v1 = v0.replace("{ 1 }", "{ 123456789 }");
    let result = h.update("m.rock", &v1);
    assert_eq!(
        result.tier,
        UpdateTier::BodyOnly,
        "a body-length edit is BodyOnly"
    );
    h.assert_clean();

    // The shift is real: same node text, +8 byte offset.
    let flo = v1.find("3.14").expect("float literal present");
    assert_eq!(
        flo,
        flo0 + 8,
        "the edit must shift the float literal by 8 bytes"
    );
    assert_eq!(
        type_and_span_at(&h, "m.rock", flo),
        Some(("float".to_string(), (flo, flo + "3.14".len()))),
        "post-shift: float literal span start must track the +8 delta exactly"
    );

    // A node in an EVEN-LATER body (booly) is shifted by the same delta; pin it
    // too so the gate covers more than the first sibling below the edit.
    let boo = v1.find("true").expect("bool literal present");
    assert_eq!(
        type_and_span_at(&h, "m.rock", boo),
        Some(("bool".to_string(), (boo, boo + "true".len()))),
        "post-shift: a second later body's node span must also track the delta"
    );
}

// ---------------------------------------------------------------------------
// Warm-module behavior: the stdlib is lowered + inferred once at
// Session::new and never again.
// ---------------------------------------------------------------------------

fn stdlib_warm_modules() -> Vec<skull_parser::Ast> {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let file = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, file)
                .unwrap_or_else(|e| panic!("stdlib {} must parse: {e:?}", f.name))
        })
        .collect()
}

#[test]
fn warm_stdlib_is_never_reinferred_by_updates() {
    let warm = stdlib_warm_modules();
    let warm_count = warm.len();
    let mut session = Session::new(
        warm,
        files(&[(
            "main.rock",
            "fn main() { let xs = vec_new() \n xs.push(1) \n print(\"{xs[0]}\") }",
        )]),
        &bone_core::modules::NsAliases::default(),
    )
    .expect("session with warm stdlib");
    assert!(
        session.diagnostics().is_empty(),
        "stdlib-using program must check clean: {:?}",
        session.diagnostics().first().map(|e| &e.message)
    );

    let result = session
        .update_file(
            "main.rock",
            "fn main() { let xs = vec_new() \n xs.push(2) \n print(\"{xs[0] + 1}\") }",
        )
        .expect("update");

    let ws = session.workspace();
    let touched_warm: Vec<_> = result
        .reinferred
        .iter()
        .filter(|(mid, _)| is_stdlib_mid(ws, *mid))
        .collect();
    assert!(
        touched_warm.is_empty(),
        "warm stdlib bodies must not be re-inferred on a user edit: {touched_warm:?}"
    );

    // The warm inferences are still present and serving queries.
    let ws = session.workspace();
    let stdlib_infs = ws
        .inferences_iter()
        .filter(|(mid, _)| is_stdlib_mid(ws, *mid))
        .count();
    assert_eq!(
        stdlib_infs, warm_count,
        "every warm stdlib module keeps its inference across updates"
    );
}

// ---------------------------------------------------------------------------
// Eviction-precision tests: a body edit re-infers only the edited module; an
// interface edit re-infers only the edited module + its name-dependents.
// ---------------------------------------------------------------------------

/// A body edit re-lowers and re-infers only the edited body: not its
/// siblings in the module, and not a caller in another file.
#[test]
fn body_edit_reinfers_and_relowers_only_the_edited_body() {
    let mut h = Harness::new(&[
        (
            "m.rock",
            "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }",
        ),
        ("n.rock", "fn d() -> int { c() }"),
    ]);
    let result = h.update(
        "m.rock",
        "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 30 }",
    );
    assert_eq!(result.tier, UpdateTier::BodyOnly);
    assert_eq!(h.function_names(&result.reinferred), ["c"]);
    assert_eq!(h.function_names(&result.relowered), ["c"]);
    assert_eq!(result.reinferred.len(), 1);
}

/// A body edit drops the dependency it removed, while a remaining field access
/// still invalidates its caller when the field type changes. The harness checks
/// the session against a fresh batch build after every edit.
#[test]
fn body_edit_dependency_removal_preserves_field_dependency() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    let b_mid = h.mid("b.rock");
    let no_factory_call = CROSS_FILE_B.replace("double_x(make_point(0, 0))", "42");
    let body_edit = h.update("b.rock", &no_factory_call);
    assert_eq!(body_edit.tier, UpdateTier::BodyOnly);
    h.assert_clean();

    let changed_factory = CROSS_FILE_A.replace(
        "fn make_point(x: int, y: int) -> Point {",
        "fn make_point(x: int, y: int, z: int) -> Point {",
    );
    let factory_edit = h.update("a.rock", &changed_factory);
    assert_eq!(factory_edit.tier, UpdateTier::Interface);
    assert!(
        !Harness::reinferred_mids(&factory_edit).contains(&b_mid),
        "b.rock no longer depends on make_point"
    );
    h.assert_clean();

    let changed_field = changed_factory.replace("x: int,", "x: string,");
    let field_edit = h.update("a.rock", &changed_field);
    assert_eq!(field_edit.tier, UpdateTier::Interface);
    assert!(
        Harness::reinferred_mids(&field_edit).contains(&b_mid),
        "b.rock still depends on Point through double_x's field access"
    );
    h.assert_diagnostic("b.rock: ");

    h.update("a.rock", CROSS_FILE_A);
    h.assert_clean();
}

/// A signature or field-shape edit to a.rock re-infers b.rock, which calls
/// `make_point` and reads `Point.x`, and leaves c.rock, which names neither.
#[test]
fn interface_edit_reinfers_only_dependents() {
    let edits = [
        "struct Point {\n    x: int,\n    y: int,\n}\n\nfn make_point(x: int, y: int, z: int) -> Point {\n    Point { x: x, y: y }\n}\n",
        "struct Point {\n    x: int,\n    y: int,\n    z: int,\n}\n\nfn make_point(x: int, y: int) -> Point {\n    Point { x: x, y: y, z: 0 }\n}\n",
    ];
    for edit in edits {
        let mut h = Harness::new(&[
            ("a.rock", CROSS_FILE_A),
            ("b.rock", CROSS_FILE_B),
            ("c.rock", "fn unrelated() -> int { 7 }"),
        ]);
        let result = h.update("a.rock", edit);
        let mids = Harness::reinferred_mids(&result);
        assert_eq!(result.tier, UpdateTier::Interface);
        assert!(mids.contains(&h.mid("b.rock")), "{edit}");
        assert!(!mids.contains(&h.mid("c.rock")), "{edit}");
    }
}

#[test]
fn consume_annotation_edit_rechecks_dependent_bodies() {
    let without_consume = "struct Token { id: int }\nfn spend(t: Token) -> int { t.id }\n";
    let with_consume = "@consume struct Token { id: int }\nfn spend(t: Token) -> int { t.id }\n";
    let consumer = "fn twice() {\n    let token = Token { id: 1 }\n    let _ = spend(token)\n    let _ = spend(token)\n}\n";
    let mut h = Harness::new(&[("a.rock", without_consume), ("b.rock", consumer)]);
    h.assert_clean();

    let result = h.update("a.rock", with_consume);
    assert_eq!(result.tier, UpdateTier::Interface);
    assert!(
        result
            .reinferred
            .iter()
            .any(|(mid, owner)| *mid == h.mid("b.rock")
                && matches!(owner, bone_semantic::hir::BodyOwner::Function(_))),
        "the Token consumer body must be re-inferred: {:?}",
        result.reinferred
    );
    h.assert_diagnostic("consum");
}

// A parameter added to `shifted` moves every later entry of the module's type
// arena, and `b.rock` holds inferred types that point into it. Those callers
// must stay cached (they read nothing the edit changed) and still read valid
// types, through repeated edits and the whole-module path a later edit takes.
#[test]
fn signature_edit_reinfers_only_the_callers_of_the_edited_function() {
    let a = |extra: &str, body: &str| {
        format!(
            "fn shifted(x: int{extra}) -> int {{ x }}\n\n\
             struct Pair<T> {{ a: T }}\n\n\
             fn stable(v: Pair<int>) -> Pair<int> {{ v }}\n\n\
             fn other(v: Pair<int>) -> int {{ {body} }}\n"
        )
    };
    let b = "fn calls_stable(p: Pair<int>) -> int { let q = stable(p)\n q.a }";
    let c = "fn calls_shifted() -> int { shifted(1) }";
    let d = "fn unrelated() -> int { 7 }";
    let mut h = Harness::new(&[
        ("a.rock", &a("", "v.a")),
        ("b.rock", b),
        ("c.rock", c),
        ("d.rock", d),
    ]);

    let result = h.update("a.rock", &a(", y: int = 0", "v.a"));
    assert_eq!(result.tier, UpdateTier::Interface);
    let mids = Harness::reinferred_mids(&result);
    assert!(mids.contains(&h.mid("c.rock")), "c.rock calls shifted");
    assert!(
        !mids.contains(&h.mid("b.rock")),
        "b.rock reads only `stable` and `Pair`, which the edit left alone"
    );
    h.assert_matches_batch();

    let result = h.update("a.rock", &a(", y: int = 0, z: int = 1", "v.a"));
    assert!(!Harness::reinferred_mids(&result).contains(&h.mid("b.rock")));
    h.assert_matches_batch();

    // Two bodies at once: the whole-module path re-lowers the file while b.rock
    // keeps its inference.
    let two_bodies = a(", y: int = 0, z: int = 1", "v.a + 1").replace("{ x }", "{ x + 1 }");
    let result = h.update("a.rock", &two_bodies);
    assert_eq!(result.tier, UpdateTier::BodyOnly);
    h.assert_matches_batch();

    let result = h.update(
        "b.rock",
        "fn calls_stable(p: Pair<int>) -> int { let q = stable(p)\n q.a + 1 }",
    );
    assert_eq!(result.tier, UpdateTier::BodyOnly);
    h.assert_matches_batch();
    h.assert_clean();
}

#[test]
fn signature_edit_reports_only_the_bodies_it_rechecked() {
    let mut h = Harness::new(&[
        ("a.rock", "fn edited(x: int) -> int { x }\n"),
        (
            "b.rock",
            "fn calls_edited() -> int { edited(1) }\nfn leaves_it_alone() -> int { 2 }\n",
        ),
    ]);

    let result = h.update("a.rock", "fn edited(x: int, y: int = 0) -> int { x }\n");

    let b = h.mid("b.rock");
    let in_b: Vec<_> = result.reinferred.iter().filter(|(m, _)| *m == b).collect();
    assert_eq!(in_b.len(), 1, "only calls_edited was re-checked: {in_b:?}");
    h.assert_matches_batch();
}

// A signature edit that adds a function renumbers the later functions, and a
// cached caller stores its callee's index: it has to re-infer.
#[test]
fn signature_edit_that_adds_a_function_reinfers_callers_of_the_later_ones() {
    let without = "fn first() -> int { 1 }\nfn later() -> int { 2 }\n";
    let with = "fn first() -> int { 1 }\nfn added() -> int { 3 }\nfn later() -> int { 2 }\n";
    let mut h = Harness::new(&[
        ("a.rock", without),
        ("b.rock", "fn use_later() -> int { later() }"),
    ]);

    let result = h.update("a.rock", with);
    assert_eq!(result.tier, UpdateTier::Scope);
    assert!(Harness::reinferred_mids(&result).contains(&h.mid("b.rock")));
    h.assert_matches_batch();
}

/// An edit that changes what a cross-file user reads -- a global's type, a
/// macro's expansion, a newtype's wrapped type -- must reach that user, which
/// now errors. A missed dependency keeps its stale clean inference and the
/// oracle inside `update` diverges. A macro error is reported at the call
/// site, since expansion retags expanded spans to the invocation.
#[test]
fn interface_edit_reaches_cross_file_users() {
    let cases = [
        (
            "global G: int = 0",
            "global G: float = 0.0",
            "fn use_g() -> int { G }",
        ),
        (
            "macro twice(e: expr) { (e) + (e) }\nfn base() -> int { 3 }",
            "macro twice(e: expr) { (e) + 1.0 }\nfn base() -> int { 3 }",
            "fn quad() -> int { twice(base()) }",
        ),
        (
            "newtype Meters(int)",
            "newtype Meters(float)",
            "fn unwrap(m: Meters) -> int { m.0 }",
        ),
    ];
    for (before, after, user) in cases {
        let mut h = Harness::new(&[("f.rock", before), ("user.rock", user)]);
        h.assert_clean();
        h.update("f.rock", after);
        h.assert_diagnostic("user.rock: ");
    }
}

// ---------------------------------------------------------------------------
// Fuzz oracle: random body / signature / delete / restore mutations over a
// generated multi-file model, incremental==fresh checked after EVERY step
// (the rock lesson: synthetic single-shot tests missed the deletion bug;
// only the per-step comparison catches a missed invalidation). Runs under
// debug assertions, so the generation tripwires double as a detector once
// the narrowing starts bumping generations.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn pick(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn fuzz_seed() -> u64 {
    std::env::var("ROCK_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(42)
}

#[derive(Clone)]
enum FuzzBody {
    Const(i64),
    Param,
    AddConst(i64),
    /// Call `f<file>_<slot>` with the arg count the TARGET had at generation
    /// time; later signature mutations may break the call — a legitimate
    /// state whose diagnostics both sides must agree on.
    Call {
        file: usize,
        slot: usize,
        args: usize,
    },
    GlobalRef(usize),
}

#[derive(Clone)]
struct FuzzFn {
    /// 1 or 2 int params; param 1 is `a`.
    arity: usize,
    body: FuzzBody,
}

#[derive(Clone)]
struct FuzzFile {
    present: bool,
    global: Option<i64>,
    /// Slot -> fn (None = deleted). At least one fn stays present per
    /// present file so the file never renders item-less.
    fns: Vec<Option<FuzzFn>>,
}

struct FuzzModel {
    files: Vec<FuzzFile>,
}

impl FuzzModel {
    fn path(file: usize) -> String {
        format!("fz{file}.rock")
    }

    fn render(&self, file: usize) -> String {
        let f = &self.files[file];
        let mut out = format!("// fuzz file {file}\n");
        if let Some(v) = f.global {
            out.push_str(&format!("global FG{file}: int = {v}\n"));
        }
        for (slot, slot_fn) in f.fns.iter().enumerate() {
            let Some(func) = slot_fn else { continue };
            let params = match func.arity {
                1 => "a: int",
                _ => "a: int, b: int",
            };
            let body = match &func.body {
                FuzzBody::Const(c) => format!("{c}"),
                FuzzBody::Param => "a".to_string(),
                FuzzBody::AddConst(c) => format!("a + {c}"),
                FuzzBody::Call { file, slot, args } => {
                    let arg_list = match args {
                        1 => "a".to_string(),
                        _ => "a, 2".to_string(),
                    };
                    format!("f{file}_{slot}({arg_list})")
                }
                FuzzBody::GlobalRef(file) => format!("FG{file}"),
            };
            out.push_str(&format!(
                "\nfn f{file}_{slot}({params}) -> int {{\n    {body}\n}}\n"
            ));
        }
        out
    }

    fn random_body(&self, rng: &mut Rng) -> FuzzBody {
        match rng.pick(5) {
            0 => FuzzBody::Const(rng.pick(100) as i64),
            1 => FuzzBody::Param,
            2 => FuzzBody::AddConst(rng.pick(100) as i64),
            3 => {
                // Target any slot of any PRESENT file, with the target's
                // CURRENT arity (the call may rot later — intended).
                let file = rng.pick(self.files.len());
                let f = &self.files[file];
                if !f.present {
                    return FuzzBody::Const(7);
                }
                let slot = rng.pick(f.fns.len());
                match &f.fns[slot] {
                    Some(target) => FuzzBody::Call {
                        file,
                        slot,
                        args: target.arity,
                    },
                    None => FuzzBody::Const(7),
                }
            }
            _ => {
                let file = rng.pick(self.files.len());
                if self.files[file].present && self.files[file].global.is_some() {
                    FuzzBody::GlobalRef(file)
                } else {
                    FuzzBody::Const(3)
                }
            }
        }
    }
}

/// One mutation step: pick something, change it, and return a description
/// (used in the panic message so a failing seed/step is reproducible).
fn fuzz_step(model: &mut FuzzModel, rng: &mut Rng, h: &mut Harness) -> String {
    let file = rng.pick(model.files.len());
    let kind = rng.pick(10);
    let desc;

    // File-level delete/restore (rarer than item mutations).
    if kind == 0 && model.files[file].present {
        // Only delete if some OTHER file stays present (keep the workspace
        // non-empty).
        if model
            .files
            .iter()
            .enumerate()
            .any(|(i, f)| i != file && f.present)
        {
            model.files[file].present = false;
            h.remove(&FuzzModel::path(file));
            return format!("delete file {file}");
        }
    }
    if !model.files[file].present {
        model.files[file].present = true;
        desc = format!("restore file {file}");
        let text = model.render(file);
        h.update(&FuzzModel::path(file), &text);
        return desc;
    }

    let slots = model.files[file].fns.len();
    let slot = rng.pick(slots);
    match kind {
        1 => {
            // Delete fn (keep at least one present fn in the file).
            let present_fns = model.files[file].fns.iter().filter(|f| f.is_some()).count();
            if present_fns > 1 && model.files[file].fns[slot].is_some() {
                model.files[file].fns[slot] = None;
                desc = format!("delete fn f{file}_{slot}");
            } else {
                model.files[file].fns[slot] = Some(FuzzFn {
                    arity: 1,
                    body: FuzzBody::Param,
                });
                desc = format!("restore fn f{file}_{slot}");
            }
        }
        2 => {
            // Signature edit: toggle arity (body kept valid - params only
            // grow; callers recorded with the old arity may break).
            let new_arity = 1 + rng.pick(2);
            match &mut model.files[file].fns[slot] {
                Some(f) => {
                    f.arity = new_arity;
                    desc = format!("set arity of f{file}_{slot} to {new_arity}");
                }
                None => {
                    model.files[file].fns[slot] = Some(FuzzFn {
                        arity: new_arity,
                        body: FuzzBody::Const(1),
                    });
                    desc = format!("recreate fn f{file}_{slot} arity {new_arity}");
                }
            }
        }
        3 => {
            // Global add/remove/retarget.
            let f = &mut model.files[file];
            if f.global.is_some() && rng.pick(2) == 0 {
                f.global = None;
                desc = format!("remove global FG{file}");
            } else {
                let v = rng.pick(1000) as i64;
                f.global = Some(v);
                desc = format!("set global FG{file} = {v}");
            }
        }
        _ => {
            // Body edit (most common, mirrors real editing).
            let body = model.random_body(rng);
            match &mut model.files[file].fns[slot] {
                Some(f) => f.body = body,
                None => {
                    model.files[file].fns[slot] = Some(FuzzFn { arity: 1, body });
                }
            }
            desc = format!("edit body of f{file}_{slot}");
        }
    }

    let text = model.render(file);
    h.update(&FuzzModel::path(file), &text);
    desc
}

/// One seeded fuzz run: generate a 3-file model, apply `steps` random
/// mutations, and assert incremental == fresh after EVERY step (inside
/// `h.update`/`h.remove`). Panics with the seed + last mutations on divergence.
fn run_fuzz_session(seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);

    let mut model = FuzzModel {
        files: (0..3)
            .map(|_| FuzzFile {
                present: true,
                global: Some(5),
                fns: vec![
                    Some(FuzzFn {
                        arity: 1,
                        body: FuzzBody::Param,
                    }),
                    Some(FuzzFn {
                        arity: 1,
                        body: FuzzBody::Const(1),
                    }),
                    Some(FuzzFn {
                        arity: 2,
                        body: FuzzBody::AddConst(2),
                    }),
                ],
            })
            .collect(),
    };

    let warm = warm_prelude();
    let initial: Files = (0..model.files.len())
        .map(|i| common::v1(&FuzzModel::path(i), &model.render(i)))
        .collect();
    let mut h = Harness::with_warm(vec![warm], initial, 1);

    let mut log: Vec<String> = Vec::new();
    for step in 0..steps {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fuzz_step(&mut model, &mut rng, &mut h)
        }));
        match result {
            Ok(desc) => log.push(desc),
            Err(panic) => {
                let tail: Vec<_> = log.iter().rev().take(5).collect();
                eprintln!(
                    "fuzz_session_synthetic failed at step {step} (seed {seed}); \
                     last mutations (newest first): {tail:?}"
                );
                std::panic::resume_unwind(panic);
            }
        }
    }
}

// Sweep many seeds for op-sequence breadth (add/remove/edit interleavings
// exercise module-id reassignment, the soundness surface the module-arena
// migration touches). One test per seed so the harness runs them in parallel.
macro_rules! fuzz_session_synthetic_seeds {
    ($($name:ident = $seed:literal),* $(,)?) => {$(
        #[test]
        fn $name() {
            run_fuzz_session($seed, 120);
        }
    )*};
}

fuzz_session_synthetic_seeds! {
    fuzz_session_synthetic_seed1 = 1,
    fuzz_session_synthetic_seed2 = 2,
    fuzz_session_synthetic_seed3 = 3,
    fuzz_session_synthetic_seed4 = 4,
    fuzz_session_synthetic_seed5 = 5,
    fuzz_session_synthetic_seed6 = 6,
    fuzz_session_synthetic_seed7 = 7,
    fuzz_session_synthetic_seed8 = 8,
    fuzz_session_synthetic_seed9 = 9,
    fuzz_session_synthetic_seed10 = 10,
    fuzz_session_synthetic_seed11 = 11,
    fuzz_session_synthetic_seed12 = 12,
    fuzz_session_synthetic_seed13 = 13,
    fuzz_session_synthetic_seed14 = 14,
    fuzz_session_synthetic_seed15 = 15,
    fuzz_session_synthetic_seed16 = 16,
    fuzz_session_synthetic_seed17 = 17,
    fuzz_session_synthetic_seed18 = 18,
    fuzz_session_synthetic_seed19 = 19,
    fuzz_session_synthetic_seed20 = 20,
    fuzz_session_synthetic_seed21 = 21,
    fuzz_session_synthetic_seed22 = 22,
    fuzz_session_synthetic_seed23 = 23,
    fuzz_session_synthetic_seed24 = 24,
    fuzz_session_synthetic_seed25 = 25,
    fuzz_session_synthetic_seed26 = 26,
    fuzz_session_synthetic_seed27 = 27,
    fuzz_session_synthetic_seed28 = 28,
    fuzz_session_synthetic_seed29 = 29,
    fuzz_session_synthetic_seed30 = 30,
    fuzz_session_synthetic_seed31 = 31,
    fuzz_session_synthetic_seed32 = 32,
}

/// Reproduce one seed outside the default sweep: `ROCK_TEST_SEED=<n>`.
#[test]
#[ignore = "set ROCK_TEST_SEED=<n> and run with --ignored"]
fn fuzz_session_synthetic_env_seed() {
    let seed = std::env::var("ROCK_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .expect("ROCK_TEST_SEED=<n> selects the seed");
    run_fuzz_session(seed, 120);
}

/// Package-graph fuzz with the full warm stdlib, gated like the rock-side real
/// fuzz: ROCK_FUZZ_REAL=1. Append/edit/revert text mutations only (no file
/// deletion - real signatures cross files, and bone's interface lowering
/// fail-fasts on those; the synthetic fuzz owns deletion). Coarse profile
/// stride keeps wall time reasonable.
///
/// `true` when `text` carries a `#script` directive line, the files
/// production dependency discovery skips. A line scan, not the preprocessor
/// lexer; close enough for a tripwire (the fixture graph has none today).
fn is_script_source(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        t == "#script" || t.starts_with("#script ") || t.starts_with("#script\t")
    })
}

#[test]
#[ignore = "requires ROCK_FUZZ_REAL=1; run explicitly with --ignored"]
fn fuzz_session_fixture_package_graph() {
    if std::env::var("ROCK_FUZZ_REAL").is_err() {
        eprintln!("fuzz_session_fixture_package_graph skipped (set ROCK_FUZZ_REAL=1)");
        return;
    }

    // This local two-package graph exercises production manifest discovery,
    // package aliases, file selection, and per-file compiler metadata. Only
    // root-package files are mutated, as in a real project edit.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/incremental_package/app");
    let graph =
        rock_host::stick::graph::PackageGraph::build(&manifest_dir).expect("fixture package graph");
    let root = graph.root();
    let mut all_files: Files = Vec::new();
    let mut originals: Files = Vec::new();
    for id in graph.load_order() {
        let node = graph.package(*id);
        // The root package loads from the invoked directory; dependencies
        // always load their whole root.
        let load_dir = if *id == root {
            manifest_dir.clone()
        } else {
            node.root.clone()
        };
        let selected = graph
            .selected_rock_files(*id, &load_dir)
            .expect("selected rock files");
        for path in selected {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            // Production skips `#script` files in dependencies
            // (`unified_compile`); none exist in this closure today, so a
            // newcomer fails loudly rather than silently diverging.
            assert!(
                *id == root || !is_script_source(&text),
                "dependency {} gained a #script file; mirror unified_compile's is_script skip",
                path.display()
            );
            let f = bone_incremental::session::SessionFile {
                path: path.to_string_lossy().to_string(),
                text,
                package: Some(common::package_tag(node.id.0, &node.self_name)),
                global_policy: node.global_policy,
            };
            if *id == root {
                originals.push(f.clone());
            }
            all_files.push(f);
        }
    }
    assert!(
        !originals.is_empty(),
        "fixture package graph has no root .rock files (guards against a silent skip)"
    );
    assert!(
        all_files.len() > originals.len(),
        "fixture graph must include a dependency package"
    );

    let aliases = graph.ns_aliases();
    let mut h = Harness::with_warm_aliases(stdlib_warm_modules(), all_files, 13, aliases);

    let seed = fuzz_seed();
    let mut rng = Rng::new(seed);
    for step in 0..8 {
        let picked = &originals[rng.pick(originals.len())];
        let (path, original) = (&picked.path, &picked.text);
        match rng.pick(3) {
            0 => {
                let text = format!(
                    "{original}\nfn __fuzz_probe_{step}() -> int {{\n    {}\n}}\n",
                    rng.pick(100)
                );
                h.update(path, &text);
            }
            1 => {
                let text = format!("{original}\n// fuzz comment {step}\n");
                h.update(path, &text);
            }
            _ => {
                h.update(path, original);
            }
        }
    }
    // Leave every file back at its original text.
    for f in &originals {
        let (path, original) = (&f.path, &f.text);
        h.update(path, original);
    }
}

// Repro for the LSP crash at rebuild_module's interface self-check
// (session.rs:720). The body-edit FAST PATH (`relower_function_in_place`)
// re-lowers the edited fn's SIGNATURE, appending its nominal param/return types
// to the module `ty_arena` and orphaning the old entries -- so a re-lowered fn's
// param `Idx<TypeExpr>` shifts and the arena grows even though the signature is
// textually unchanged. A later MULTI-item body edit takes `rebuild_module`,
// whose `debug_assert_eq!` compared the now-bloated cached interface against a
// fresh clean lowering via raw Debug (indices + the whole arena) and panicked.
// The self-check must compare type SHAPES, not arena indices. Nominal-typed
// params (`P`) are required: builtin (`int`) params don't allocate into the
// module arena, so they never trigger the drift.
#[test]
fn fastpath_arena_drift_is_not_an_interface_change() {
    let v = |fb: &str, gb: &str| {
        format!(
            "
struct P {{ x: int }}

fn f(p: P) -> int {{
    return{fb}p.x
}}

fn g(p: P) -> int {{
    return{gb}p.x
}}
"
        )
    };

    let mut h = Harness::new(&[("a.rock", &v(" ", " "))]);

    // 1) Single-fn body edit -> fast path -> `relower_function_in_place` bloats
    //    `ty_arena` (appends `f`'s `P` param, orphans the old idx).
    let r1 = h.update("a.rock", &v("  ", " "));
    assert_eq!(r1.tier, UpdateTier::BodyOnly);

    // 2) Multi-fn body edit (both f and g change vs the stored edit-1 state) ->
    //    `rebuild_module`, which runs the interface self-check against the
    //    bloated cached arena. Must NOT report an interface change.
    let r2 = h.update("a.rock", &v(" ", "  "));
    assert_eq!(r2.tier, UpdateTier::BodyOnly);
}

// A body-only edit must NOT grow the module `ty_arena`. The fast path
// (`rebuild_body_incremental` -> `bone-lower`'s `relower_function_in_place`)
// reuses the unchanged signature's interface types (`lower_fn_body_into` only
// allocates into the fresh `Body`) instead of re-lowering the signature -- which
// would append fresh entries to `module.ty_arena` and orphan the old ones,
// leaking one slot per edit and shifting the re-lowered fn's param
// `Idx<TypeExpr>`. Nominal-typed params (`P`) are required: builtin (`int`)
// params resolve to a shared builtin `TyId` and never allocate into the module
// arena, so they wouldn't exercise the leak. Was a sentinel for the leak; flipped
// to the correct assertion when the bone-lower reuse landed.
#[test]
fn body_edit_does_not_grow_module_ty_arena() {
    let v = |fb: &str| {
        format!(
            "
struct P {{ x: int }}
fn f(p: P) -> int {{
    return{fb}p.x
}}
"
        )
    };
    let mut h = Harness::new(&[("a.rock", &v(" "))]);
    let mid = h.mid("a.rock");
    let start = h.session.workspace().module(mid).ty_arena.len();
    for i in 0..20 {
        let pad = " ".repeat(1 + (i % 3));
        let _ = h.session.update_file("a.rock", &v(&pad)).unwrap();
    }
    let after = h.session.workspace().module(mid).ty_arena.len();

    assert_eq!(
        start, after,
        "body-only edits must not grow the module ty_arena (start={start}, \
         after 20 edits={after}); the fast path must reuse the unchanged \
         signature's interface types, not re-lower them"
    );
}

// STRESS (gated; `--ignored`). Empirical backstop for "memory does not grow with
// edits": 100k single-function body edits must leave every per-module collection
// flat. Arenas are the deterministic oracle (RSS is allocator-noisy). Covers the
// leak-prone shape (nominal-typed signatures across several fns). The cheap
// `body_edit_does_not_grow_module_ty_arena` is the always-on version.
#[test]
#[ignore = "stress test: 100k edits take minutes; run with --ignored"]
fn stress_100k_edits_arena_flat() {
    let v = |b: &str| {
        format!(
            "struct P {{ x: int }}\n\
         fn f(p: P) -> P {{\n{b}\n    return p\n}}\n\
         fn g(p: P) -> int {{ return p.x }}\n\
         fn h(a: P, b: P) -> int {{ return a.x + b.x }}\n"
        )
    };
    // Each edit changes a literal in `f`. The per-edit assertion below pins
    // that every update really re-infers it, so flat arenas cannot come from
    // a no-change fast path.
    let body = |k: usize| format!("    let y: P = p\n    let z: int = p.x + {k}");
    let mut h = Harness::new(&[("a.rock", &v(&body(0)))]);
    let mid = h.mid("a.rock");

    // settle one edit so lazily-grown per-body arenas reach steady state.
    h.session.update_file("a.rock", &v(&body(9))).unwrap();
    let snap = |h: &Harness| {
        let m = h.session.workspace().module(mid);
        let body: usize = m
            .bodies
            .values()
            .map(|b| b.lowered_tys.len() + b.expr_arena.len() + b.node_intervals.len())
            .sum();
        (
            m.ty_arena.len(),
            m.ty_spans.iter().count(),
            m.lowering_errors.len(),
            m.functions.len(),
            m.bodies.len(),
            body,
        )
    };
    let baseline = snap(&h);

    for i in 0..100_000 {
        h.session
            .update_file("a.rock", &v(&body(1 + i % 4)))
            .unwrap();
        assert_eq!(
            h.session
                .ws
                .reinferred_body_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "edit {i} must re-infer the edited body"
        );
    }

    let after = snap(&h);
    assert_eq!(
        baseline, after,
        "100k clean body edits must leave all per-module collections flat \
         (ty_arena, ty_spans, lowering_errors, functions, bodies, sum_body_arenas): \
         baseline={baseline:?} after={after:?}"
    );
}

// Regression for the lower-error-path leak + stale-diagnostic bug. A body-only
// relower mutates the module in place; the old flat `lowering_errors` Vec was
// append-only, so toggling a body through a LOWERING error (unknown type
// annotation) accumulated errors AND left them stale after the body was fixed.
// Now keyed by body and REPLACED on each relower. The `Harness` oracle compares
// incremental vs fresh diagnostics after every edit, so a stale/duplicate error
// diverges; the explicit assertion pins the no-accumulation + clear-on-fix.
#[test]
fn body_lower_errors_are_replaced_not_accumulated() {
    let v = |b: &str| format!("struct P {{ x: int }}\nfn f(p: P) -> int {{\n{b}\n}}\n");
    let ok = v("    return p.x");
    let bad = v("    let q: Nope = p\n    return p.x"); // `Nope` is a LOWERING error

    let mut h = Harness::new(&[("a.rock", &ok)]);
    let mid = h.mid("a.rock");
    let total = |h: &Harness| -> usize {
        h.session
            .workspace()
            .module(mid)
            .lowering_errors
            .values()
            .map(|v| v.len())
            .sum()
    };
    assert_eq!(total(&h), 0);

    // Toggle bad <-> ok many times; the oracle validates diagnostics each step.
    for _ in 0..25 {
        h.update("a.rock", &bad);
        assert_eq!(
            total(&h),
            1,
            "the single lowering error must not accumulate across edits"
        );
        h.update("a.rock", &ok);
        assert_eq!(
            total(&h),
            0,
            "fixing the body must CLEAR its stale lowering error"
        );
    }
}

// A global's INIT VALUE is its body: editing it (same type) is a BodyOnly edit,
// so a `const` hot-reloads through the cheap overlay instead of a full rebuild.
// The global's whole-item text was previously the iface_hash (no body_hash), so
// any value edit mis-classified as Interface (full `main.lua` rebuild).
#[test]
fn const_value_edit_is_body_only() {
    use bone_semantic::hir::BodyOwner;
    let mut h = Harness::new(&[("m.rock", "const FOO: int = 5\nfn f() -> int { FOO }")]);
    let result = h.update("m.rock", "const FOO: int = 10\nfn f() -> int { FOO }");
    assert_eq!(
        result.tier,
        UpdateTier::BodyOnly,
        "editing a const's value is a body edit"
    );
    assert!(
        result
            .reinferred
            .iter()
            .any(|(_, owner)| matches!(owner, BodyOwner::Global(_))),
        "the changed const must appear in reinferred, got: {:?}",
        result.reinferred
    );
}

#[test]
fn value_and_keyword_edits_on_globals_are_interface() {
    let cases: &[(&str, &str, &str)] = &[
        // A mutable global's value edit stays Interface (full rebuild): its live value is
        // preserved across reload, so the value edit has no live effect -- no overlay.
        (
            "m.rock",
            "global BAR: int = 5\nfn f() -> int { BAR }",
            "global BAR: int = 7\nfn f() -> int { BAR }",
        ),
        // Control: changing a global's TYPE is still an Interface edit (the type is in
        // the iface_hash). A const->global keyword flip is likewise interface.
        (
            "m.rock",
            "const FOO: int = 5\nfn f() {}",
            "const FOO: float = 5.0\nfn f() {}",
        ),
        // const->global flip changes write/reload semantics
        (
            "m.rock",
            "const FOO: int = 5\nfn f() {}",
            "global FOO: int = 5\nfn f() {}",
        ),
    ];
    for (path, before, after) in cases {
        let mut h = Harness::new(&[(path, before)]);
        let result = h.update(path, after);
        assert_eq!(result.tier, UpdateTier::Interface, "{before}");
    }
}

/// ALIASING is whole-program: editing only the callee's file must change the
/// verdict at the unchanged caller's element-reference call, both ways.
#[test]
fn relocation_verdict_follows_an_edit_to_the_callee() {
    let clean_callee =
        "value Cell { v: int }\nfn touch(cell: ref Cell, cells: [Cell]) { cell.v = cells.len() }";
    let growing_callee = "value Cell { v: int }\nfn touch(cell: ref Cell, cells: [Cell]) { cells.push(Cell { v: 2 })\n cell.v = 7 }";
    let caller = "fn main() { let cells: [Cell] = [Cell { v: 1 }]\n touch(cells[0], cells) }";
    let relocations = |session: &Session| {
        session
            .diagnostics()
            .iter()
            .filter(|e| e.message.contains("relocatable Vec or Map element"))
            .count()
    };
    let mut session = Session::new(
        stdlib_warm_modules(),
        files(&[("cell.rock", clean_callee), ("main.rock", caller)]),
        &bone_core::modules::NsAliases::default(),
    )
    .expect("session");
    assert_eq!(relocations(&session), 0, "{:?}", session.diagnostics());

    session
        .update_file("cell.rock", growing_callee)
        .expect("update");
    assert_eq!(relocations(&session), 1, "{:?}", session.diagnostics());

    session
        .update_file("cell.rock", clean_callee)
        .expect("update");
    assert_eq!(relocations(&session), 0, "{:?}", session.diagnostics());
}

/// A diff that starts and ends inside one body can still add an item: the
/// common suffix of `return 0\n}` and `return 2\n}` puts the whole insertion
/// inside `lonely`'s old body. The single-item reparse stops after `lonely`,
/// so the fast path must notice the unparsed `trailing` and take the full
/// route instead of dropping it.
#[test]
fn oracle_body_edit_that_appends_an_item_is_not_body_only() {
    let v1 = "fn added(q: int) -> int {\n    return lonely(q)\n}\n\nfn lonely(z: int) -> int {\n    let idle = z\n    return 0\n}\n";
    let mut h = Harness::new(&[("c.rock", v1)]);
    let v2 = "fn added(q: int) -> int {\n    return lonely(q)\n}\n\nfn lonely(z: int) -> int {\n    return z\n}\n\nfn trailing() -> int {\n    let tail = 1\n    return 2\n}\n";
    let result = h.update("c.rock", v2);
    assert_ne!(result.tier, UpdateTier::BodyOnly);
}

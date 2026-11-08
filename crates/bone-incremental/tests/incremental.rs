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

use bone_incremental::session::{
    Session, UpdateResult, UpdateTier, changed_names, item_prints, name_shapes,
};
use bone_semantic::prelude::*;

use crate::common;
use common::{mid_for, type_profile};

fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from(file))
}

// ---------------------------------------------------------------------------
// Harness: parse + batch check
// ---------------------------------------------------------------------------

type Files = Vec<bone_incremental::session::SessionFile>;

fn files(list: &[(&str, &str)]) -> Files {
    list.iter()
        .map(|(p, t)| bone_incremental::session::SessionFile::legacy(*p, *t))
        .collect()
}

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
        let mut module =
            skull_parser::parse_module_with_dialect(text, Ustr::from(path.as_str()), f.dialect)?;
        // The oracle must see what the session sees: `Session::new` stamps
        // these same facts on every parse. A no-op for legacy fixtures
        // (Legacy/None/Game/Any are the parser defaults).
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
/// table for real-project sets, default (empty) for legacy fixtures.
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

#[test]
fn comparator_equal_programs_compare_equal() {
    let fs = files(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    let left =
        batch_check(&[], &fs, &bone_core::modules::NsAliases::default()).expect("batch check");
    let right =
        batch_check(&[], &fs, &bone_core::modules::NsAliases::default()).expect("batch check");
    ws_equiv(&left, &right, &fs, 1).expect("identical batch runs must compare equal");
}

#[test]
fn comparator_detects_type_divergence() {
    // Same shape, different inferred type at the same offsets: `1` vs `1.0`
    // (padded so both files have identical length and the literal sits at the
    // same byte offset — only the TYPE differs, not node positions).
    let int_version = files(&[("a.rock", "fn f() -> int   { let v = 1   \n v }")]);
    let float_version = files(&[("a.rock", "fn f() -> float { let v = 1.0 \n v }")]);

    let left = batch_check(&[], &int_version, &bone_core::modules::NsAliases::default())
        .expect("batch check");
    let right = batch_check(
        &[],
        &float_version,
        &bone_core::modules::NsAliases::default(),
    )
    .expect("batch check");

    let err = ws_equiv(&left, &right, &int_version, 1)
        .expect_err("comparator must detect differing inferred types");
    assert!(
        err.contains("type diverges"),
        "expected a type divergence report, got: {err}"
    );
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
    /// The host graph's alias tables (default/empty for legacy fixtures).
    aliases: bone_core::modules::NsAliases,
    /// The language server's lint cache, fed the change each update implies;
    /// it must agree with a fresh lint pass after every step.
    lints: bone_lints::IncrementalLintCache,
}

impl Harness {
    fn new(initial: &[(&str, &str)]) -> Harness {
        let warm =
            parse_rock_with_file(WARM_PRELUDE, WARM_PRELUDE_PATH).expect("warm prelude parses");
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
        let result = self
            .session
            .update_file(path, text)
            .unwrap_or_else(|e| panic!("update_file({path}) failed: {}", e.message));
        match self.texts.iter_mut().find(|f| f.path == path) {
            Some(f) => f.text = text.to_string(),
            None => self
                .texts
                .push(bone_incremental::session::SessionFile::legacy(path, text)),
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
        before[0].span.line_range.0, 4,
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
        after[0].span.line_range.0, 5,
        "macro def-line must follow the +1 line shift (was 4, now 5)"
    );
}

/// A `co fn` body edit must re-lower as a COROUTINE. Footgun guarded here: the
/// parser advances past `co` before `parse_fn`, so the function's item span
/// starts at `fn`, and the fast-path body reparse (`try_incremental_body_edit`)
/// slices `text[item_span]` from `fn` -- the re-parsed item is a PLAIN `fn`.
/// `is_coroutine` survives today only because `relower_function_in_place` reuses
/// the cached signature; this pins that invariant (and the incremental==fresh
/// oracle inside `update` catches any body mislowering of `yield`).
#[test]
fn oracle_coroutine_body_edit_stays_a_coroutine() {
    let v1 = "co fn counter() -> Coroutine<int> {\n  yield 1\n}\n";
    let mut h = Harness::new(&[("c.rock", v1)]);
    h.assert_clean();
    let result = h.update(
        "c.rock",
        "co fn counter() -> Coroutine<int> {\n  yield 2\n}\n",
    );
    assert_eq!(
        result.tier,
        UpdateTier::BodyOnly,
        "a co fn body edit is BodyOnly"
    );
    h.assert_clean();
    // The re-lowered function must still be a coroutine.
    let ws = h.session.workspace();
    let mid = h.mid("c.rock");
    let m = ws.modules.get(mid.index()).unwrap();
    let (_, fns) = m
        .functions_by_name
        .iter()
        .find(|(n, _)| n.as_str() == "counter")
        .unwrap();
    let (fmid, fid) = fns[0];
    assert!(
        ws.modules[fmid.index()].functions[fid].is_coroutine,
        "counter must remain a coroutine after a body edit"
    );
}

#[test]
fn oracle_body_edit() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    let result = h.update(
        "a.rock",
        "
struct Point {
    x: int,
    y: int,
}

fn make_point(x: int, y: int) -> Point {
    Point { x: x + 1, y: y + 1 }
}
",
    );
    // Body-only edit (same items + signatures) takes the single-module rebuild.
    assert_eq!(result.tier, UpdateTier::BodyOnly);
    assert!(!result.reinferred.is_empty());
}

/// PRECISION (premise-TDD for per-body `reinferred`): a body edit should report
/// ONLY the edited body in `reinferred`, not every body in the module.
///
/// `classify` already pinpoints the changed items (`FileDiff::BodiesOnly(Vec<
/// usize>)`), but `rebuild_module` re-infers and REPORTS the whole edited module
/// (`collect_reinferred(&mids)`), so this currently returns all three fns.
/// skull's `emit_incremental` re-emits every body in `reinferred` (+ its inline-
/// dependent callers), so a whole-module report defeats within-module incremental
/// codegen on a single big `main.rock`. Narrowing the report to the changed
/// bodies (rung 1b -- `fc049f8e1` -- now narrows the re-inference WORK too, not
/// just the report) turns this green; skull's codegen-dep expansion still catches
/// inline-dependent callers, so emit stays correct (verified by the
/// incremental==fresh oracles).
#[test]
fn body_edit_reinfers_only_the_edited_body() {
    let v1 = "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }";
    let mut h = Harness::new(&[("m.rock", v1)]);
    let result = h.update(
        "m.rock",
        "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 30 }",
    );
    assert_eq!(result.tier, UpdateTier::BodyOnly);

    let ws = h.session.workspace();
    let mut names: Vec<String> = result
        .reinferred
        .iter()
        .filter_map(|(mid, owner)| match owner {
            bone_semantic::hir::BodyOwner::Function(fid) => {
                Some(ws.module(*mid).functions[*fid].name.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names,
        vec!["c".to_string()],
        "a body edit should re-infer only the edited body `c`, got {names:?}"
    );
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
    assert_eq!(
        result.tier,
        UpdateTier::BodyOnly,
        "a decorated body edit is still a body edit"
    );

    let ws = h.session.workspace();
    let mut names: Vec<String> = result
        .reinferred
        .iter()
        .filter_map(|(mid, owner)| match owner {
            bone_semantic::hir::BodyOwner::Function(fid) => {
                Some(ws.module(*mid).functions[*fid].name.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names,
        vec!["tick".to_string()],
        "a decorated body edit must re-infer the edited `tick`, got {names:?}"
    );
}

/// MILESTONE 1, step 1 (deterministic): a body edit must BUMP the changed body's
/// `Body.generation` and PRESERVE the unchanged bodies' generations. This is the
/// primitive `check_module` keys reuse on (`body_generation == body.generation`)
/// and that lowering keeps stable for unchanged bodies.
#[test]
fn body_edit_bumps_only_changed_body_generation() {
    let mut h = Harness::new(&[(
        "m.rock",
        "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }",
    )]);
    let body_gen = |h: &Harness, name: &str| -> u64 {
        let mid = h.mid("m.rock");
        let module = h.session.workspace().module(mid);
        let fid = module
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == name)
            .map(|(fid, _)| fid)
            .expect("fn exists");
        module.bodies[&bone_semantic::hir::BodyOwner::Function(fid)].generation
    };

    let (a0, b0) = (body_gen(&h, "a"), body_gen(&h, "b"));
    // Edit only `a`'s body.
    h.update(
        "m.rock",
        "fn a() -> int { 99 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }",
    );
    assert!(
        body_gen(&h, "a") > a0,
        "changed body `a` must bump its generation (was {a0}, still {})",
        body_gen(&h, "a")
    );
    assert_eq!(
        body_gen(&h, "b"),
        b0,
        "unchanged body `b` must preserve its generation"
    );
}

/// MILESTONE 1, step 1 (overloads): generations must be keyed by FUNCTION
/// IDENTITY, not name -- two overloads share a name, so a name-keyed capture
/// collides (one entry, last-write-wins). Here `foo(int)` and `foo(float)` are
/// distinct functions in source order (fids 0,1). Editing only `foo(int)`'s body
/// twice must drive ITS generation to 2 while `foo(float)` stays at 0. A
/// name-keyed capture sticks `foo(int)` at 1 (it re-reads `foo(float)`'s gen each
/// edit). Read generations in source order so the two overloads are kept apart.
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

/// MILESTONE 1 (inference), DETERMINISTIC: a body edit re-INFERS only the changed
/// body, reusing the cached `BodyInference` for the rest -- regardless of module
/// size. Reads `ws.reinferred_body_count` (the actual count of `check_function`
/// calls) instead of a phase-timing ratio. The old timing version was flaky:
/// `check_module_incremental` iterates ALL bodies for the generation-check, so the
/// "infer functions" phase scales O(N) with module size even when only one body
/// re-infers -- under parallel load the 20x size ratio crossed the threshold. The
/// counter measures the work that actually matters (check_function calls), so a
/// regression to whole-module re-inference makes it `== n` instead of `== 1`.
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

/// MILESTONE 2 (the full win, per-body LOWERING): a body edit must RE-LOWER only
/// the edited body, reusing the rest of the module's HIR with their spans shifted
/// by the edit's text delta. DETERMINISTIC gate on `UpdateResult.relowered` --
/// replaces the old flaky lower+infer timing ratio (`min_phase_ms` over the
/// `"lower functions"` phase), which needed loosening to absorb O(N) bookkeeping
/// noise and never failed cleanly.
///
/// A single-function body edit takes `rebuild_body_incremental`
/// (`relower_function_in_place` for the edited fn + `shift_module_after_edit` for
/// the kept items), so `relowered` reports exactly the edited body. Span
/// correctness for the kept bodies is guarded separately by
/// `body_edit_above_keeps_later_node_spans_exact`, so this pair pins both halves:
/// "only one body re-lowered" + "the others' spans are still right".
#[test]
fn body_edit_relowers_only_the_edited_body() {
    let v1 = "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }";
    let mut h = Harness::new(&[("m.rock", v1)]);
    let result = h.update(
        "m.rock",
        "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 30 }",
    );
    assert_eq!(result.tier, UpdateTier::BodyOnly);

    let ws = h.session.workspace();
    let mut names: Vec<String> = result
        .relowered
        .iter()
        .filter_map(|(mid, owner)| match owner {
            bone_semantic::hir::BodyOwner::Function(fid) => {
                Some(ws.module(*mid).functions[*fid].name.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names,
        vec!["c".to_string()],
        "a body edit must re-lower ONLY the edited body `c`, got {names:?}"
    );
}

#[test]
fn oracle_signature_edit_breaks_then_fixes_cross_file_caller() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);

    // make_point loses a param: b.rock's two-arg call site goes stale.
    // (An int->float param change would NOT break the caller - int literals
    // coerce to float.)
    h.update(
        "a.rock",
        "
struct Point {
    x: int,
    y: int,
}

fn make_point(x: int) -> Point {
    Point { x: x, y: x }
}
",
    );
    assert!(
        !h.session.diagnostics().is_empty(),
        "caller in b.rock must error after the signature change"
    );

    // Fix the caller; everything recovers.
    h.update(
        "b.rock",
        "
fn double_x(p: Point) -> int {
    p.x * 2
}

fn origin_doubled() -> int {
    double_x(make_point(0))
}
",
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
    let warm = parse_rock_with_file(WARM_PRELUDE, WARM_PRELUDE_PATH).expect("warm parses");
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
    let without_next = "\
struct R { hi: int }
struct RIt { cur: int, hi: int }
fn iter(r: R) -> RIt { RIt { cur: 0, hi: r.hi } }
fn total() -> int { let s = 0; for x in (R { hi: 3 }) { s = s + x } s }
";
    h.update("it.rock", without_next);
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
    assert!(
        h.session
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("Point")),
        "removing Point's definition surfaces b.rock's unknown-type error: {:?}",
        h.session
            .diagnostics()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );
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
    assert!(
        h.session
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("Nonexistent")),
        "the unresolved signature type surfaces: {:?}",
        h.session
            .diagnostics()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );

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
    assert!(
        h.session
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("Point")),
        "b.rock's signature reference to the removed Point must surface: {:?}",
        h.session
            .diagnostics()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );

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
    let a = "struct T { v: int }";
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
    assert!(
        h.session
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("Point")),
        "b's reference to the old name must surface: {:?}",
        h.session
            .diagnostics()
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
    );

    h.update("b.rock", "fn px(p: Pointe) -> int { p.x }"); // fix the reference
    h.assert_clean();
}

#[test]
fn oracle_user_macro_cross_file_edit() {
    let macro_def = "
macro twice(e: expr) {
    (e) + (e)
}

fn base() -> int { 3 }
";
    let mut h = Harness::new(&[
        ("m.rock", macro_def),
        ("use.rock", "fn quad() -> int { twice(base()) }"),
    ]);
    h.assert_clean();

    // Edit the macro definition: the user of the macro must see the change
    // (registry is rebuilt from unexpanded modules every rebuild).
    h.update(
        "m.rock",
        "
macro twice(e: expr) {
    (e) + (e) + (e)
}

fn base() -> int { 3 }
",
    );
    h.assert_clean();
}

/// The incremental-cook path: a BODY edit of a file that USES a macro must
/// re-cook just that file against the CACHED registry (`cook_one`) and still
/// match a fresh batch. `oracle_user_macro_cross_file_edit` edits the macro
/// DEF, which routes to the full `rebuild_users` (registry rebuilt); this hits
/// the `BodyOnly` tier that reuses the cached registry -- the case the
/// per-file-cook optimization introduced.
#[test]
fn oracle_body_edit_of_macro_user_recooks_against_cached_registry() {
    let macro_def = "
macro twice(e: expr) {
    (e) + (e)
}

fn base() -> int { 3 }
";
    let mut h = Harness::new(&[
        ("m.rock", macro_def),
        ("use.rock", "fn quad() -> int { twice(base()) }"),
    ]);
    h.assert_clean();

    // Signature unchanged -> BodyOnly: `cook_one` re-expands ONLY use.rock's
    // `twice(base())` against the cached registry. The Harness asserts
    // incremental == fresh batch after the update.
    let r = h.update("use.rock", "fn quad() -> int { twice(base()) + 1 }");
    assert_eq!(
        r.tier,
        UpdateTier::BodyOnly,
        "a macro-user body edit should be BodyOnly"
    );
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
    // hand-running `rock watch example --backend bone`.
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
        "
global GRID_W: int = 8

fn area() -> int {
    GRID_W * GRID_W * 1
}
",
    );
    h.assert_clean();

    // And a third, through the OTHER rebuild path (file add).
    h.update("h.rock", "fn use_area() -> int { area() }");
    h.assert_clean();
}

// ---------------------------------------------------------------------------
// Classifier table tests: item_prints + classify are pure functions,
// so detection correctness is tested directly, not through session behavior.
// ---------------------------------------------------------------------------

fn prints_of(src: &str) -> Vec<bone_incremental::session::ItemPrint> {
    let module = parse_rock_with_file(src, "cls.rock").expect("classifier fixture parses");
    bone_incremental::session::item_prints(&module, src)
}

fn classify_texts(old: &str, new: &str) -> bone_incremental::session::FileDiff {
    bone_incremental::session::classify(&prints_of(old), &prints_of(new))
}

#[test]
fn classify_body_edit_is_bodies_only_with_right_index() {
    use bone_incremental::session::FileDiff;
    let old = "
fn first() -> int {
    1
}

fn second() -> int {
    2
}
";
    let new = "
fn first() -> int {
    1
}

fn second() -> int {
    222
}
";
    let diff = classify_texts(old, new);
    assert!(
        matches!(&diff, FileDiff::BodiesOnly(v) if v.as_slice() == [1]),
        "body edit of the second fn must be BodiesOnly([1]), got {diff:?}"
    );
}

#[test]
fn classify_comment_between_items_is_layout_only() {
    use bone_incremental::session::FileDiff;
    let old = "
fn a() -> int {
    1
}

fn b() -> int {
    2
}
";
    let new = "
fn a() -> int {
    1
}

// an explanatory comment

fn b() -> int {
    2
}
";
    let diff = classify_texts(old, new);
    assert!(
        matches!(diff, FileDiff::LayoutOnly),
        "top-level comment insertion must be LayoutOnly, got {diff:?}"
    );
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

#[test]
fn classify_interface_shaped_edits() {
    use bone_incremental::session::FileDiff;
    let base = "
fn a(x: int) -> int {
    x
}

fn b() -> int {
    2
}
";
    let cases: &[(&str, &str)] = &[
        // signature edit
        (
            "sig",
            "
fn a(x: int, y: int) -> int {
    x
}

fn b() -> int {
    2
}
",
        ),
        // annotation added
        (
            "annotation",
            "
@test
fn a(x: int) -> int {
    x
}

fn b() -> int {
    2
}
",
        ),
        // item added
        (
            "added",
            "
fn a(x: int) -> int {
    x
}

fn b() -> int {
    2
}

fn c() -> int {
    3
}
",
        ),
        // item removed
        (
            "removed",
            "
fn a(x: int) -> int {
    x
}
",
        ),
        // reorder
        (
            "reorder",
            "
fn b() -> int {
    2
}

fn a(x: int) -> int {
    x
}
",
        ),
    ];
    for (label, new) in cases {
        let diff = classify_texts(base, new);
        assert!(
            matches!(diff, FileDiff::Interface),
            "{label} edit must classify Interface, got {diff:?}"
        );
    }
}

#[test]
fn classify_macro_body_edit_is_interface() {
    use bone_incremental::session::FileDiff;
    // Macro bodies are interface: expansion bakes them into every caller.
    let old = "
macro twice(e: expr) {
    (e) + (e)
}

fn user() -> int {
    1
}
";
    let new = "
macro twice(e: expr) {
    (e) + (e) + (e)
}

fn user() -> int {
    1
}
";
    let diff = classify_texts(old, new);
    assert!(
        matches!(diff, FileDiff::Interface),
        "macro body edit must classify Interface, got {diff:?}"
    );
}

#[test]
fn classify_mutable_global_value_edit_is_interface() {
    use bone_incremental::session::FileDiff;
    // A mutable `global` keeps whole-item interface (its value is preserved across
    // reload, so a value edit has no live effect) -- a value edit is Interface.
    // Only `const` globals split body/iface (see classify_const_value_edit_is_body).
    let diff = classify_texts(
        "global G: int = 1\n\nfn f() -> int {\n    G\n}\n",
        "global G: int = 2\n\nfn f() -> int {\n    G\n}\n",
    );
    assert!(
        matches!(diff, FileDiff::Interface),
        "mutable global initializer edit classifies Interface, got {diff:?}"
    );
}

#[test]
fn classify_const_value_edit_is_body_only() {
    use bone_incremental::session::FileDiff;
    // A `const`'s init VALUE is its body (keyword/name/type stay interface), so a
    // value-only edit is BodiesOnly -- it hot-reloads via the overlay.
    let diff = classify_texts(
        "const G: int = 1\n\nfn f() -> int {\n    G\n}\n",
        "const G: int = 2\n\nfn f() -> int {\n    G\n}\n",
    );
    assert!(
        matches!(diff, FileDiff::BodiesOnly(_)),
        "const initializer edit classifies BodiesOnly, got {diff:?}"
    );
}

#[test]
fn classify_overload_body_edit_targets_right_overload() {
    use bone_incremental::session::FileDiff;
    let old = "
fn pick(x: int) -> int {
    1
}

fn pick(x: int, y: int) -> int {
    2
}
";
    let new = "
fn pick(x: int) -> int {
    1
}

fn pick(x: int, y: int) -> int {
    22
}
";
    let diff = classify_texts(old, new);
    assert!(
        matches!(&diff, FileDiff::BodiesOnly(v) if v.as_slice() == [1]),
        "editing the second overload's body must be BodiesOnly([1]), got {diff:?}"
    );
}

// ---------------------------------------------------------------------------
// Per-body eviction hazard fixtures: green under the current module rebuild,
// these become the real guards once single-body eviction lands. Each targets
// one failure mode of partial re-lowering.
// ---------------------------------------------------------------------------

#[test]
fn per_body_hazard_edit_above_shifts_sibling_spans() {
    // Editing the FIRST body with a length change shifts every span below
    // it in the same file. Under per-body eviction the second body is not re-lowered,
    // so its node_index / expr_spans / diagnostics offsets must be
    // delta-corrected - the per-offset type profile and the (file, offset)
    // diagnostic keys both diverge from batch if they are not.
    let two_bodies = "
fn first() -> int {
    1
}

fn second() -> int {
    first()
}
";
    let mut h = Harness::new(&[("s.rock", two_bodies)]);
    h.assert_clean();

    // Length-changing body edit above `second` (1 -> 100000).
    h.update(
        "s.rock",
        "
fn first() -> int {
    100000
}

fn second() -> int {
    first()
}
",
    );
    h.assert_clean();

    // Same again but the sibling below now has a type ERROR, so its stored
    // diagnostic offset must track the shift too.
    h.update(
        "s.rock",
        "
fn first() -> int {
    1
}

fn second() -> int {
    true
}
",
    );
    assert_eq!(h.session.diagnostics().len(), 1);
    h.update(
        "s.rock",
        "
fn first() -> int {
    123456789
}

fn second() -> int {
    true
}
",
    );
    assert_eq!(h.session.diagnostics().len(), 1);
}

#[test]
fn per_body_hazard_generic_fn_and_hole_annotation_share_module() {
    // Batch lowering draws interface Generic VarIds and in-body `_`/hole
    // Infer VarIds from ONE module-wide counter, so they never collide. A
    // single-body relower with a fresh counter can mint Infer(VarId(n))
    // colliding with the module's Generic(VarId(n)) - and
    // BodyInference.substitutions is a single VarId keyspace. Editing
    // `holes` must not disturb `ident`'s generic instantiation (or vice
    // versa).
    let src = "
fn ident<T>(x: T) -> T {
    x
}

fn holes() -> int {
    let v: _ = 41
    v
}

fn uses_ident() -> int {
    ident(7)
}
";
    let mut h = Harness::new(&[("g.rock", src)]);
    h.assert_clean();

    // Body-only edit of the hole-annotated fn.
    h.update(
        "g.rock",
        "
fn ident<T>(x: T) -> T {
    x
}

fn holes() -> int {
    let v: _ = 42
    v
}

fn uses_ident() -> int {
    ident(7)
}
",
    );
    h.assert_clean();
}

#[test]
fn per_body_hazard_mixed_owner_file_body_edit() {
    // A file whose owners are not just functions: a global initializer and
    // a struct with field defaults. A body-only fn edit must leave the
    // Global / StructDefaults bodies untouched but still correct.
    let src = "
global BASE: int = 10

struct Conf {
    scale: int = 3,
}

fn use_conf(c: Conf) -> int {
    c.scale + BASE
}
";
    let mut h = Harness::new(&[("m.rock", src)]);
    h.assert_clean();

    h.update(
        "m.rock",
        "
global BASE: int = 10

struct Conf {
    scale: int = 3,
}

fn use_conf(c: Conf) -> int {
    c.scale + BASE + BASE
}
",
    );
    h.assert_clean();
}

/// `type_at` at `offset` in `path` as `(rendered type, span.offset_range)`.
/// Reads the stored `expr_spans` for the node under the cursor -- exactly the
/// thing rung 2's per-body span-patching has to keep correct.
fn type_and_span_at(h: &Harness, path: &str, offset: usize) -> Option<(String, (usize, usize))> {
    let ws = h.session.workspace();
    let mid = mid_for(ws, path);
    let inf = ws.inference(mid)?;
    let ta = bone_semantic::query::type_at(ws, mid, inf, offset)?;
    Some((ws.render(ta.body, ta.ty), ta.span.offset_range))
}

/// DETERMINISTIC span-correctness gate for rung 2 (per-body lowering;
/// span relocation invariants). A length-changing edit to an EARLIER body shifts
/// every node below it. Today the whole module re-lowers, so the later bodies'
/// spans are re-derived fresh from the new text; once rung 2 keeps unchanged
/// bodies' HIR, their stored `expr_spans` must be PATCHED by the edit's
/// text-length delta or `type_at` resolves the wrong text.
///
/// This pins the property DIRECTLY -- the exact `offset_range` of `type_at` on a
/// later node -- which is strictly stronger on the span dimension than the
/// type-only `ws_equiv` profile the Harness already runs: a span left off by the
/// un-applied delta (but still landing inside the right node's type) passes the
/// profile and fails here. Green now (whole-module relower); rung 2 must keep it
/// green for EVERY shifted body, not just the cached error bodies that
/// `per_body_hazard_edit_above_shifts_sibling_spans` exercises.
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
    let v1 = "\
fn edited() -> int { 123456789 }
fn floaty() -> float {
    let m = 3.14
    m
}
fn booly() -> bool {
    let t = true
    t
}
";
    let result = h.update("m.rock", v1);
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

/// Unit-pins `shift_module_after_edit` in ISOLATION, before it's wired into
/// `rebuild_module`. Three fns: `a` before the edit point, `b` the edited body,
/// `c` after. Shifting at/after `b`'s start must (1) leave `a` untouched,
/// (2) skip `b`'s OWN body spans (re-lowered fresh by the caller), and
/// (3) shift `c`'s body + header spans by exactly the delta. This pins the
/// gating + edited-skip logic independent of the lowering split that uses it.
#[test]
fn shift_module_after_edit_moves_only_later_item_spans() {
    use bone_semantic::hir::BodyOwner;
    let src = "fn a() -> int { 111 }\nfn b() -> int { 222 }\nfn c() -> int { 333 }";
    let fs = files(&[("m.rock", src)]);
    let ws = batch_check(&[], &fs, &bone_core::modules::NsAliases::default()).expect("batch check");
    let mid = mid_for(&ws, "m.rock");
    let mut module = ws.module(mid).clone();

    let fid = |m: &bone_semantic::hir::Module, name: &str| {
        m.functions
            .iter()
            .find(|(_, f)| f.name.as_str() == name)
            .map(|(i, _)| i)
            .expect("fn exists")
    };
    let (a, b, c) = (fid(&module, "a"), fid(&module, "b"), fid(&module, "c"));

    let body_offsets = |m: &bone_semantic::hir::Module, f| -> Vec<(usize, usize)> {
        let mut v: Vec<_> = m.bodies[&BodyOwner::Function(f)]
            .expr_spans
            .values()
            .map(|s| s.offset_range)
            .collect();
        v.sort();
        v
    };

    let a_hdr0 = module.functions[a].span.offset_range;
    let a_body0 = body_offsets(&module, a);
    let b_hdr0 = module.functions[b].span.offset_range;
    let b_body0 = body_offsets(&module, b);
    let c_hdr0 = module.functions[c].span.offset_range;
    let c_body0 = body_offsets(&module, c);

    // Shift everything at/after `b`'s start by +100 bytes / +5 lines; `b` is the
    // re-lowered owner.
    let at = b_hdr0.0;
    let d = 100usize;
    bone_incremental::span_shift::shift_module_after_edit(
        &mut module,
        BodyOwner::Function(b),
        at,
        d as isize,
        5,
    );

    let bump = |v: &[(usize, usize)]| -> Vec<(usize, usize)> {
        v.iter().map(|(s, e)| (s + d, e + d)).collect()
    };

    // (1) `a` is entirely before `at`: untouched, header and body.
    assert_eq!(
        module.functions[a].span.offset_range, a_hdr0,
        "earlier fn `a` header must not move"
    );
    assert_eq!(
        body_offsets(&module, a),
        a_body0,
        "earlier fn `a` body must not move"
    );

    // (2) `b` is the edited owner: its BODY spans are skipped (caller re-lowers
    //     them fresh), even though its header sits at `at` and shifts.
    assert_eq!(
        body_offsets(&module, b),
        b_body0,
        "edited fn `b` body spans must be left for re-lowering"
    );
    assert_eq!(
        module.functions[b].span.offset_range,
        (b_hdr0.0 + d, b_hdr0.1 + d),
        "edited fn `b` header (start == at) still shifts"
    );

    // (3) `c` is a kept body after the edit: header AND body shift by the delta.
    assert_eq!(
        module.functions[c].span.offset_range,
        (c_hdr0.0 + d, c_hdr0.1 + d),
        "later fn `c` header must shift by the delta"
    );
    assert_eq!(
        body_offsets(&module, c),
        bump(&c_body0),
        "later fn `c` body spans must shift by the delta"
    );
}

/// Isolated parity gate for `relower_function_in_place` (rung 2's lowering
/// primitive), before `rebuild_module` depends on it. Re-lowering one function
/// against its OWN unchanged (cooked) source must produce a module
/// indistinguishable from a fresh full lower -- same diagnostics, same `type_at`
/// at every offset. This pins the primitive's correctness (sig reuse, var-counter
/// seeding, in-place overwrite) and exercises the swap-the-module-out-of-`ws`
/// borrow dance that the session wiring will use.
#[test]
fn relower_function_in_place_matches_fresh_lower() {
    // `a` is a kept function (relower must not disturb it); `target` carries a
    // param + a let-binding + block so the re-lower rebuilds real body structure.
    let src = "fn a(x: int) -> int { x }\nfn target(y: int) -> int { let z = y\n z }";
    let fs = files(&[("m.rock", src)]);

    // `semantic_pass` cooks the modules IN PLACE, so after it `modules` holds the
    // cooked ASTs the workspace was lowered from -- exactly what relower needs.
    let mut modules = parse_files(&fs).expect("parse");
    let mut ws = semantic_pass(&mut modules).expect("semantic pass");
    let cooked = &modules[0];
    let mid = mid_for(&ws, "m.rock");

    // target's cooked FunctionDef + annotations, and its fid in the module.
    let (f, annos) = cooked
        .items
        .iter()
        .find_map(|it| match &it.kind {
            skull_parser::ItemKind::Function(f) if f.name.name.as_str() == "target" => {
                Some((f, it.annotations.as_slice()))
            }
            _ => None,
        })
        .expect("target fn in cooked ast");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, fnc)| fnc.name.as_str() == "target")
        .map(|(i, _)| i)
        .expect("target fid");

    // Wipe target's body so relower must REBUILD it -- otherwise the test is
    // vacuous (relowering same source onto an already-correct module is
    // indistinguishable from a no-op). With the body gone, a relower that fails
    // to install leaves target body-less and ws_equiv diverges from fresh.
    ws.module_mut(mid)
        .bodies
        .remove(&bone_semantic::hir::BodyOwner::Function(fid));
    assert!(
        !ws.module(mid)
            .bodies
            .contains_key(&bone_semantic::hir::BodyOwner::Function(fid)),
        "precondition: target body removed"
    );

    // Swap the module out so `&ws` (workspace lookups) and `&mut module` don't
    // alias -- the exact dance rebuild_module will perform.
    let mut module = std::mem::take(ws.module_mut(mid));
    let installed =
        bone_semantic::lower::relower_function_in_place(&ws, &mut module, cooked, f, annos, fid)
            .expect("relower must succeed");
    assert!(
        installed,
        "target has a Rock body, so a body must be installed"
    );
    *ws.module_mut(mid) = module;

    // Re-infer (the relowered body's old inference is stale) and compare to a
    // fresh batch lower of the same files: relower(same source) == fresh lower.
    let inf =
        bone_semantic::semantic_passes::check_module(&ws, mid, ws.module(mid)).expect("reinfer");
    ws.set_inference(mid, inf);

    let fresh =
        batch_check(&[], &fs, &bone_core::modules::NsAliases::default()).expect("fresh batch");
    ws_equiv(&ws, &fresh, &fs, 1)
        .expect("relower_function_in_place(same cooked source) must equal a fresh full lower");
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

#[test]
fn body_edit_reinfers_only_edited_module() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);

    // Body-only edit in a.rock: same item list, same signatures.
    let result = h.update(
        "a.rock",
        "
struct Point {
    x: int,
    y: int,
}

fn make_point(x: int, y: int) -> Point {
    Point { x: y, y: x }
}
",
    );

    let mids = Harness::reinferred_mids(&result);

    assert_eq!(result.tier, UpdateTier::BodyOnly);
    assert!(
        !mids.contains(&h.mid("b.rock")),
        "body edit in a.rock must not re-infer b.rock"
    );
    // Module-granular eviction re-infers every body in a.rock, not just the one
    // changed body. Tighten to `reinferred.len() == 1` (exactly make_point) once
    // per-body eviction lands.
    assert!(
        result
            .reinferred
            .iter()
            .all(|(mid, _)| *mid == h.mid("a.rock")),
        "body edit: only a.rock's bodies re-infer; got {mids:?}"
    );
}

// Per-body name-dep recording (observe-only). The reverse
// index isn't wired into eviction yet; this just pins that the recording sees
// both a call name AND a field-access struct name (the dep that is invisible
// in the source - b.rock's `origin_doubled` reaches a Point field purely
// through inference, never naming Point in a type).
#[test]
fn dep_index_records_cross_module_names() {
    let h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);

    let b = h.session.module_deps(h.mid("b.rock"));
    assert!(
        b.contains(&Ustr::from("Point")),
        "field access p.x must record Point; got {b:?}"
    );
    assert!(
        b.contains(&Ustr::from("make_point")),
        "call must record make_point; got {b:?}"
    );

    // a.rock defines Point/make_point; it must not depend on b.rock's names.
    let a = h.session.module_deps(h.mid("a.rock"));
    assert!(
        !a.contains(&Ustr::from("double_x")),
        "a.rock must not depend on b.rock's double_x; got {a:?}"
    );
}

/// A body edit that CHANGES which names a module references must incrementally
/// patch the reverse dep index (`update_dependents_for`, the body-edit fast path)
/// -- drop the names the edit removed, keep the rest -- WITHOUT an O(workspace)
/// rescan. Pins both the specific transition and that the patched index equals a
/// full rebuild.
#[test]
fn body_edit_incrementally_updates_dep_index() {
    let mut h = Harness::new(&[("a.rock", CROSS_FILE_A), ("b.rock", CROSS_FILE_B)]);
    let b_mid = h.mid("b.rock");
    let has = |h: &Harness, name: &str| {
        h.session
            .dependents
            .get(&Ustr::from(name))
            .is_some_and(|s| s.contains(&b_mid))
    };

    assert!(has(&h, "make_point"), "precondition: b calls make_point");
    assert!(has(&h, "Point"), "precondition: b reaches a Point field");

    // Single-body edit: origin_doubled no longer calls make_point (double_x's
    // `p.x` keeps the Point dep). BodyOnly -> rebuild_body_incremental ->
    // update_dependents_for(b).
    let r = h.update(
        "b.rock",
        "
fn double_x(p: Point) -> int {
    p.x * 2
}

fn origin_doubled() -> int {
    42
}
",
    );
    assert_eq!(r.tier, UpdateTier::BodyOnly);

    assert!(
        !has(&h, "make_point"),
        "make_point dep must be dropped after the edit"
    );
    assert!(
        has(&h, "Point"),
        "Point dep (via double_x `p.x`) must remain"
    );

    // The incrementally-patched index must be byte-identical to a full rescan
    // (no stale/empty entries) -- the soundness invariant the body path relies on.
    let incremental = h.session.dependents.clone();
    h.session.rebuild_dependents();
    assert_eq!(
        incremental, h.session.dependents,
        "incremental dep index must equal a full rebuild"
    );
}

// Name-shape diff. Drives the helpers directly over parsed
// item_prints (no inference involved) so the changed-name set is pinned exactly.
// The body-only case is the load-bearing one: a body edit must NOT register as
// a name-shape change, or eviction would over-invalidate on every keystroke.
const SHAPE_BASE: &str = "
struct Point { x: int, y: int }
fn make_point(x: int, y: int) -> Point { Point { x: x, y: y } }
fn other() -> int { 0 }
";

fn shapes(text: &str) -> FxHashMap<Ustr, u64> {
    let m = parse_rock_with_file(text, "shape.rock").expect("parse");
    name_shapes(&item_prints(&m, text))
}

fn diff(old: &str, new: &str) -> FxHashSet<Ustr> {
    changed_names(&shapes(old), &shapes(new))
}

fn names(list: &[&str]) -> FxHashSet<Ustr> {
    list.iter().map(|s| Ustr::from(s)).collect()
}

#[test]
fn name_shape_diff_signature_edit() {
    let new = "
struct Point { x: int, y: int }
fn make_point(x: int, y: int, z: int) -> Point { Point { x: x, y: y } }
fn other() -> int { 0 }
";
    assert_eq!(diff(SHAPE_BASE, new), names(&["make_point"]));
}

#[test]
fn name_shape_diff_struct_field_add() {
    let new = "
struct Point { x: int, y: int, z: int }
fn make_point(x: int, y: int) -> Point { Point { x: x, y: y } }
fn other() -> int { 0 }
";
    assert_eq!(diff(SHAPE_BASE, new), names(&["Point"]));
}

#[test]
fn name_shape_diff_add_and_remove_item() {
    let added = "
struct Point { x: int, y: int }
fn make_point(x: int, y: int) -> Point { Point { x: x, y: y } }
fn other() -> int { 0 }
fn fresh() -> int { 1 }
";
    assert_eq!(diff(SHAPE_BASE, added), names(&["fresh"]));

    let removed = "
struct Point { x: int, y: int }
fn make_point(x: int, y: int) -> Point { Point { x: x, y: y } }
";
    assert_eq!(diff(SHAPE_BASE, removed), names(&["other"]));
}

#[test]
fn name_shape_diff_body_edit_is_empty() {
    // Same signatures, different bodies: NO name-shape change.
    let new = "
struct Point { x: int, y: int }
fn make_point(x: int, y: int) -> Point { Point { x: y, y: x } }
fn other() -> int { 42 }
";
    assert_eq!(diff(SHAPE_BASE, new), FxHashSet::default());
}

#[test]
fn interface_edit_reinfers_only_dependents() {
    let mut h = Harness::new(&[
        ("a.rock", CROSS_FILE_A),
        ("b.rock", CROSS_FILE_B),
        // c.rock never mentions make_point or Point.
        ("c.rock", "fn unrelated() -> int { 7 }"),
    ]);

    // Signature edit to make_point (interface change). Only b.rock calls it.
    let result = h.update(
        "a.rock",
        "
struct Point {
    x: int,
    y: int,
}

fn make_point(x: int, y: int, z: int) -> Point {
    Point { x: x, y: y }
}
",
    );

    let mids = Harness::reinferred_mids(&result);

    // The dep index narrows the fanout to the edited module + its dependents.
    assert_eq!(result.tier, UpdateTier::Interface);
    assert!(
        mids.contains(&h.mid("b.rock")),
        "b.rock calls make_point, so a signature change must re-infer it"
    );
    assert!(
        !mids.contains(&h.mid("c.rock")),
        "c.rock observes neither make_point nor Point - must stay untouched"
    );
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
    assert_eq!(result.tier, UpdateTier::Interface);
    assert!(Harness::reinferred_mids(&result).contains(&h.mid("b.rock")));
    h.assert_matches_batch();
}

#[test]
fn interface_edit_field_add_reinfers_struct_users_only() {
    let mut h = Harness::new(&[
        ("a.rock", CROSS_FILE_A),
        ("b.rock", CROSS_FILE_B),
        ("c.rock", "fn unrelated() -> int { 7 }"),
    ]);

    // Add a field to Point - a struct-shape change. b.rock touches `p.x`
    // (a Point field) so it must re-infer even though it never edits; c.rock
    // never names Point.
    let result = h.update(
        "a.rock",
        "
struct Point {
    x: int,
    y: int,
    z: int,
}

fn make_point(x: int, y: int) -> Point {
    Point { x: x, y: y, z: 0 }
}
",
    );

    let mids = Harness::reinferred_mids(&result);
    assert_eq!(result.tier, UpdateTier::Interface);
    assert!(
        mids.contains(&h.mid("b.rock")),
        "b.rock reads Point.x, so a field-shape change must re-infer it"
    );
    assert!(
        !mids.contains(&h.mid("c.rock")),
        "c.rock never touches Point - must stay untouched"
    );
}

#[test]
fn interface_edit_global_type_change_reinfers_readers() {
    // F changes a global's type; a cross-module reader must re-infer (and now
    // error). If global reads aren't recorded as deps, the reader keeps its
    // stale (clean) inference and the oracle inside update() diverges.
    let mut h = Harness::new(&[
        ("f.rock", "global G: int = 0"),
        ("b.rock", "fn use_g() -> int { G }"),
    ]);
    h.update("f.rock", "global G: float = 0.0");
    let diags = h.session.diagnostics();
    assert!(
        diags.iter().any(|e| e.location.file.as_str() == "b.rock"),
        "global type change must re-infer b.rock and surface its error; got {diags:?}"
    );
}

#[test]
fn interface_edit_macro_body_change_relowers_users() {
    // A macro change alters the EXPANSION of its users, so they must be
    // re-lowered (re-expanded), not merely re-inferred. Type-visible edit: the
    // macro starts producing a float, so the user's int-typed fn goes
    // ill-typed. If macro users aren't re-lowered, use.rock keeps its stale
    // expansion (int) and the oracle diverges from a fresh batch.
    let mut h = Harness::new(&[
        (
            "m.rock",
            "macro twice(e: expr) { (e) + (e) }\nfn base() -> int { 3 }",
        ),
        ("use.rock", "fn quad() -> int { twice(base()) }"),
    ]);
    h.update(
        "m.rock",
        "macro twice(e: expr) { (e) + 1.0 }\nfn base() -> int { 3 }",
    );
    let diags = h.session.diagnostics();
    // The macro change re-expands use.rock and the now-`(base()) + 1.0` is
    // caught as a float/int mismatch, reported at the call site `use.rock` —
    // skull's macro expansion retags expanded-node spans to the invocation site
    // (`retag_block` in skull-parser's `macro_expand`).
    assert!(
        diags.iter().any(|e| e.location.file.as_str() == "use.rock"),
        "expected the now-float macro error to surface at the call site use.rock; got {diags:?}"
    );
}

#[test]
fn interface_edit_newtype_wrapped_change_reinfers_users() {
    // F changes a newtype's wrapped type; a cross-module user that unwraps it
    // must re-infer. If newtype refs aren't recorded as deps, it's skipped.
    let mut h = Harness::new(&[
        ("f.rock", "newtype Meters(int)"),
        ("b.rock", "fn unwrap(m: Meters) -> int { m.0 }"),
    ]);
    h.update("f.rock", "newtype Meters(float)");
    let diags = h.session.diagnostics();
    assert!(
        diags.iter().any(|e| e.location.file.as_str() == "b.rock"),
        "newtype wrapped-type change must re-infer b.rock; got {diags:?}"
    );
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

    let warm = parse_rock_with_file(WARM_PRELUDE, WARM_PRELUDE_PATH).expect("warm prelude parses");
    let initial: Files = (0..model.files.len())
        .map(|i| {
            bone_incremental::session::SessionFile::legacy(FuzzModel::path(i), model.render(i))
        })
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
                dialect: node.dialect,
                package: Some(skull_parser::PackageTag {
                    id: node.id.0,
                    self_name: node.self_name.clone(),
                    identity_storage: Default::default(),
                }),
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

#[test]
fn comparator_detects_diagnostic_divergence() {
    let clean = files(&[("a.rock", "fn f() -> int { 1 }")]);
    let broken = files(&[("a.rock", "fn f() -> int { true }")]);

    let left =
        batch_check(&[], &clean, &bone_core::modules::NsAliases::default()).expect("batch check");
    let right = batch_check(&[], &broken, &bone_core::modules::NsAliases::default())
        .expect("batch check (body errors are per-owner, not Err)");

    let err = ws_equiv(&left, &right, &clean, 1)
        .expect_err("comparator must detect differing diagnostics");
    assert!(
        err.contains("diagnostics diverge"),
        "expected a diagnostics divergence report, got: {err}"
    );
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

// A mutable global's value edit stays Interface (full rebuild): its live value is
// preserved across reload, so the value edit has no live effect -- no overlay.
#[test]
fn mutable_global_value_edit_is_interface() {
    let mut h = Harness::new(&[("m.rock", "global BAR: int = 5\nfn f() -> int { BAR }")]);
    let result = h.update("m.rock", "global BAR: int = 7\nfn f() -> int { BAR }");
    assert_eq!(
        result.tier,
        UpdateTier::Interface,
        "editing a mutable global's value is an interface edit"
    );
}

// Control: changing a global's TYPE is still an Interface edit (the type is in
// the iface_hash). A const->global keyword flip is likewise interface.
#[test]
fn const_type_edit_is_interface() {
    let mut h = Harness::new(&[("m.rock", "const FOO: int = 5\nfn f() {}")]);
    let result = h.update("m.rock", "const FOO: float = 5.0\nfn f() {}");
    assert_eq!(
        result.tier,
        UpdateTier::Interface,
        "changing a const's type is an interface edit"
    );
}

#[test]
fn const_to_global_keyword_flip_is_interface() {
    let mut h = Harness::new(&[("m.rock", "const FOO: int = 5\nfn f() {}")]);
    let result = h.update("m.rock", "global FOO: int = 5\nfn f() {}");
    assert_eq!(
        result.tier,
        UpdateTier::Interface,
        "const->global flip changes write/reload semantics"
    );
}

/// A `co fn` body edit must REPORT the edited body in `reinferred` -- not just
/// re-lower it correctly. skull's overlay re-emits exactly the `reinferred`
/// bodies, so a co fn missing from that list is silently dropped from the
/// incremental.lua overlay (the running game keeps the stale coroutine).
///
/// Root cause (bone-lower footprint): the parser consumes `co` before
/// `parse_fn`, so `f.span` starts at `fn` while the `Item.span` starts at `co`.
/// The `item_index` footprint is built from `f.span`, so `body_at(Item.span.0)`
/// -- the offset every body-locator uses -- falls OUTSIDE the interval, both the
/// fast path and `rebuild_body_incremental` bail to `rebuild_module(&[])`, and
/// that LayoutOnly-shaped call re-infers nothing. classify itself is correct
/// (`BodiesOnly([0])`); the miss is purely the locator/footprint mismatch.
#[test]
fn coroutine_body_edit_is_reported_in_reinferred() {
    let t0 =
        "co fn counter() -> Coroutine<int> {\n  yield 1\n}\nfn main() {\n  let c = counter()\n}\n";
    let t1 =
        "co fn counter() -> Coroutine<int> {\n  yield 10\n}\nfn main() {\n  let c = counter()\n}\n";
    let mut h = Harness::new(&[("c.rock", t0)]);
    let r = h.update("c.rock", t1);
    assert_eq!(
        r.reinferred.len(),
        1,
        "a co fn body edit must re-infer + REPORT exactly the edited body (got {} -- \
         the bone-lower item-footprint excludes the `co` prefix, so body_at misses)",
        r.reinferred.len()
    );
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

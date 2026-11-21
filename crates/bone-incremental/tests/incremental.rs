// Incremental == fresh oracle: apply an edit script, batch the final texts, compare.

use bone_incremental::session::Session;
use bone_semantic::prelude::*;

use crate::common;
use common::{batch_check, files, type_profile};

fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from(file))
}

fn warm_prelude() -> skull_parser::Ast {
    let mut warm = parse_rock_with_file(WARM_PRELUDE, WARM_PRELUDE_PATH).expect("warm parses");
    warm.package = Some(common::package_tag(1, common::PACKAGE));
    warm
}

type Files = Vec<bone_incremental::session::SessionFile>;

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

const WARM_PRELUDE_PATH: &str = "<warm-prelude>";
const WARM_PRELUDE: &str = "
fn __op_add(a: int, b: int) -> int { 0 }
fn __op_sub(a: int, b: int) -> int { 0 }
fn __op_mul(a: int, b: int) -> int { 0 }
";

struct Harness {
    session: Session,
    warm: Vec<skull_parser::Ast>,
    texts: Files,
    stride: usize,
    aliases: bone_core::modules::NsAliases,
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

    fn update(&mut self, path: &str, text: &str) {
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
    }

    fn remove(&mut self, path: &str) {
        self.session
            .remove_file(path)
            .unwrap_or_else(|e| panic!("remove_file({path}) failed: {}", e.message));
        self.texts.retain(|f| f.path != path);
        self.assert_matches_batch();
        self.lints.invalidate_file(Ustr::from(path));
        self.assert_lints_match(&bone_lints::LintCacheUpdate::body_only([], []));
    }

    fn assert_lints_match(&mut self, change: &bone_lints::LintCacheUpdate) {
        let sources: FxHashMap<Ustr, &str> = self
            .texts
            .iter()
            .map(|f| (Ustr::from(f.path.as_str()), f.text.as_str()))
            .collect();
        let ws = self.session.workspace();
        let warm = &self.session.warm_mids;
        let options = bone_lints::LintOptions::default();
        let cached = self
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

    #[track_caller]
    fn assert_clean(&self) {
        common::assert_clean(&self.session);
    }
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

    h.remove("c.rock");
    assert!(!h.session.is_tracked("c.rock"));

    h.remove("c.rock");
    assert!(!h.session.is_tracked("c.rock"));
}

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
fn oracle_macro_def_retype_then_user_body_edit_sees_new_registry() {
    let mut h = Harness::new(&[
        ("m.rock", "macro wrap(e: expr) {\n    e\n}\n"),
        ("use.rock", "fn f() -> int { wrap(5) }"),
    ]);
    h.assert_clean();

    h.update("m.rock", "macro wrap(e: expr) {\n    \"{e}\"\n}\n");

    // A stale registry would let this body edit pass.
    h.update("use.rock", "fn f() -> int { wrap(5) + 0 }");
}

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
fn consume_annotation_edit_reports_the_reuse_error() {
    let without_consume = "struct Token { id: int }\nfn spend(t: Token) -> int { t.id }\n";
    let with_consume = "@consume struct Token { id: int }\nfn spend(t: Token) -> int { t.id }\n";
    let consumer = "fn twice() {\n    let token = Token { id: 1 }\n    let _ = spend(token)\n    let _ = spend(token)\n}\n";
    let mut h = Harness::new(&[("a.rock", without_consume), ("b.rock", consumer)]);
    h.assert_clean();

    h.update("a.rock", with_consume);
    h.assert_diagnostic("consum");
}

// A new same-typed function must not change which definition the caller reaches.
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
    Call {
        file: usize,
        slot: usize,
        args: usize,
    },
    GlobalRef(usize),
}

#[derive(Clone)]
struct FuzzFn {
    arity: usize,
    body: FuzzBody,
}

#[derive(Clone)]
struct FuzzFile {
    present: bool,
    global: Option<i64>,
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
                // Uses the target's current arity; the call may rot later, which is intended.
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

fn fuzz_step(model: &mut FuzzModel, rng: &mut Rng, h: &mut Harness) -> String {
    let file = rng.pick(model.files.len());
    let kind = rng.pick(10);
    let desc;

    if kind == 0 && model.files[file].present {
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
            // Params only grow, so the body stays valid; old-arity callers may break.
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
            let body = model.random_body(rng);
            model.files[file].fns[slot]
                .get_or_insert(FuzzFn {
                    arity: 1,
                    body: FuzzBody::Param,
                })
                .body = body;
            desc = format!("edit body of f{file}_{slot}");
        }
    }

    let text = model.render(file);
    h.update(&FuzzModel::path(file), &text);
    desc
}

fn run_fuzz_session(seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);

    let mut model = FuzzModel {
        files: (0..3)
            .map(|_| FuzzFile {
                present: true,
                global: Some(5),
                fns: [
                    (1, FuzzBody::Param),
                    (1, FuzzBody::Const(1)),
                    (2, FuzzBody::AddConst(2)),
                ]
                .map(|(arity, body)| Some(FuzzFn { arity, body }))
                .into(),
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

#[test]
fn fuzz_session_synthetic_seeds() {
    (1..=32)
        .into_par_iter()
        .for_each(|seed| run_fuzz_session(seed, 120));
}

#[test]
#[ignore = "set ROCK_TEST_SEED=<n> and run with --ignored"]
fn fuzz_session_synthetic_env_seed() {
    let seed = std::env::var("ROCK_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .expect("ROCK_TEST_SEED=<n> selects the seed");
    run_fuzz_session(seed, 120);
}

#[test]
#[ignore = "requires ROCK_FUZZ_REAL=1; run explicitly with --ignored"]
fn fuzz_session_fixture_package_graph() {
    if std::env::var("ROCK_FUZZ_REAL").is_err() {
        eprintln!("fuzz_session_fixture_package_graph skipped (set ROCK_FUZZ_REAL=1)");
        return;
    }

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
    for f in &originals {
        let (path, original) = (&f.path, &f.text);
        h.update(path, original);
    }
}

#[test]
fn body_lower_errors_are_replaced_not_accumulated() {
    let v = |b: &str| format!("struct P {{ x: int }}\nfn f(p: P) -> int {{\n{b}\n}}\n");
    let ok = v("    return p.x");
    let bad = v("    let q: Nope = p\n    return p.x");

    let mut h = Harness::new(&[("a.rock", &ok)]);
    for _ in 0..25 {
        h.update("a.rock", &bad);
        h.assert_diagnostic("Nope");
        h.update("a.rock", &ok);
        h.assert_clean();
    }
}

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
    let mut session = common::with_warm(
        stdlib_warm_modules(),
        files(&[("cell.rock", clean_callee), ("main.rock", caller)]),
    );
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

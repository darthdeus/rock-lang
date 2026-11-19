//! End-to-end tests for the bone-backed LSP facade (`bone_query`): feed a
//! file in, query it, assert the LSP-shaped result.

use crate::completion::{assert_labels, complete_in, labels_of};
use crate::fixture::{PATH, Project, assert_text, diagnostics_in, hover_markup, workspace};
use crate::label_asserts::assert_offers;
use bone_query::BoneWorkspace;
use bone_query::FileUriExt;
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

#[track_caller]
fn hover_text(bone: &BoneWorkspace, pos: Position) -> String {
    hover_markup(bone, PATH, pos).unwrap_or_else(|| panic!("no hover at {pos:?}"))
}

#[test]
fn hover_cases() {
    let mob = "struct Mob { hp: int }\n";
    let signature = format!("{mob}fn use_it(x: Mob) -> Mob {{\n  x\n}}\n");
    let enum_match = "enum MobKind { Skeleton, Zombie }\nfn name(k: MobKind) -> string {\n  match k {\n    MobKind::Skeleton => \"s\"\n    MobKind::Zombie => \"z\"\n  }\n}\n";
    // (case, source, position, text fragments; `!` forbids a fragment)
    let cases: &[(&str, &str, (u32, u32), &[&str])] = &[
        (
            "uint keeps its signedness",
            "fn main() {\n  let x: uint = 42\n  x\n}\n",
            (1, 6),
            &["uint", "!Type: int"],
        ),
        (
            "type hole reports kind and scope",
            "fn main() {\n  let count = 5\n  let x: ? = count + 1\n}\n",
            (2, 9),
            &["Type hole", "count: int"],
        ),
        (
            "expression hole reports expected type and scope",
            "fn main() {\n  let a = 1\n  let x: int = ?\n}\n",
            (2, 15),
            &["Expression hole expecting: int", "a: int"],
        ),
        (
            "global declaration",
            "global score: int\nfn main() {}\n",
            (0, 9),
            &["int"],
        ),
        (
            "body type annotation",
            &format!("{mob}fn main() {{\n  let m: Mob = Mob {{ hp: 1 }}\n}}\n"),
            (2, 10),
            &["struct Mob", "hp: int"],
        ),
        (
            "return type",
            &signature,
            (1, 22),
            &["struct Mob", "hp: int"],
        ),
        // A newtype's generated `__op_add` shares the newtype's name span.
        (
            "newtype declaration",
            "newtype Health(int)\nfn main() {}\n",
            (0, 9),
            &["newtype Health(int)", "!__op"],
        ),
        (
            "record pattern head",
            &format!(
                "{mob}fn f(m: Mob) {{\n  match m {{\n    Mob {{ hp: _h }} => {{}}\n  }}\n}}\n"
            ),
            (3, 5),
            &["struct Mob"],
        ),
        (
            "enum variant in a pattern",
            enum_match,
            (3, 16),
            &["MobKind::Skeleton", "!string"],
        ),
        // A value-less global has no inference body.
        (
            "global type annotation",
            &format!("{mob}global boss: Mob\nfn main() {{}}\n"),
            (1, 14),
            &["struct Mob", "hp: int"],
        ),
        (
            "callee shows param defaults",
            "fn plot(x: int, y: int = 0) {\n}\nfn main() {\n  plot(1)\n}\n",
            (3, 3),
            &["y: int = 0"],
        ),
    ];
    for (case, src, (line, ch), patterns) in cases {
        let text = hover_text(&workspace(src), Position::new(*line, *ch));
        assert_text(case, &text, patterns);
    }
}

/// Both views of a file's diagnostics: what `update` publishes (the editor
/// path) and what `diagnostics_for` reports.
fn both_views(src: &str) -> [Vec<Diagnostic>; 2] {
    let mut bone = BoneWorkspace::new();
    let published = diagnostics_in(bone.update(PATH, src.to_string()), PATH);
    [published, bone.diagnostics_for(PATH)]
}

#[test]
fn reported_diagnostics() {
    let non_exhaustive =
        "enum E { A, B, C }\n\nfn f(e: E) {\n    match e {\n        E::A => {}\n    }\n}\n";
    // (case, source, message fragment, severity)
    let cases = [
        (
            "type error",
            "fn main() {\n  let x: int = \"hello\"\n}\n",
            "type mismatch: expected `int`, found `string`",
            DiagnosticSeverity::ERROR,
        ),
        (
            "non-exhaustive match lists missing variants in order",
            non_exhaustive,
            "Non-exhaustive match expression. Missing patterns: B, C",
            DiagnosticSeverity::ERROR,
        ),
        (
            "empty match misses every variant",
            "enum E { A, B, C }\n\nfn f(e: E) {\n    match e {\n    }\n}\n",
            "Missing patterns: A, B, C",
            DiagnosticSeverity::ERROR,
        ),
        // A guarded arm may not fire, so it does not cover its variant.
        (
            "guarded arm",
            "enum E { A, B }\n\nfn f(e: E, x: bool) {\n    match e {\n        E::A if x => {}\n        E::B => {}\n    }\n}\n",
            "Missing patterns: A",
            DiagnosticSeverity::ERROR,
        ),
        (
            "resolved type hole is a hint",
            "fn main() {\n  let x: ? = 5\n}\n",
            "type hole ? inferred as int",
            DiagnosticSeverity::HINT,
        ),
    ];
    for (case, src, needle, severity) in cases {
        for diags in both_views(src) {
            let d = diags
                .iter()
                .find(|d| d.message.contains(needle))
                .unwrap_or_else(|| panic!("{case}: no {needle:?} in {diags:?}"));
            assert_eq!(d.severity, Some(severity), "{case}: {d:?}");
            assert_eq!(d.source.as_deref(), Some("bone"), "{case}: {d:?}");
        }
    }
}

#[test]
fn unfilled_expression_hole_is_a_warning_on_the_hole() {
    for diags in both_views("fn main() {\n  let y: int = ?\n}\n") {
        let hole = diags
            .iter()
            .find(|d| d.message.contains("expression hole"))
            .unwrap_or_else(|| panic!("expr hole should warn, got: {diags:?}"));
        assert_eq!(hole.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            hole.code,
            Some(NumberOrString::String("unfilled_expression_hole".into()))
        );
        assert!(hole.message.contains("expecting int"), "{}", hole.message);
        assert_eq!(hole.source.as_deref(), Some("bone"));
        assert_eq!(hole.range.start, Position::new(1, 15), "ranged on the `?`");
    }
}

#[test]
fn quiet_diagnostics() {
    // (case, source, message fragment that must not appear; None: no diagnostics)
    let cases = [
        ("clean file", "fn f() -> int {\n  let x = 1\n  x\n}\n", None),
        (
            "stdlib-using program",
            "fn main() {\n  let xs = vec_new()\n  xs.push(1)\n}\n",
            None,
        ),
        (
            "exhaustive match",
            "enum E { A, B }\n\nfn f(e: E) {\n    match e {\n        E::A => {}\n        E::B => {}\n    }\n}\n",
            Some("Non-exhaustive"),
        ),
        (
            "catch-all match",
            "enum E { A, B }\n\nfn f(e: E) {\n    match e {\n        E::A => {}\n        _ => {}\n    }\n}\n",
            Some("Non-exhaustive"),
        ),
        (
            "filled hole",
            "fn main() {\n  let y: int = 42\n}\n",
            Some("hole"),
        ),
        // bone's @pure check is annotation-based, so the stdlib `lerp` a @pure
        // caller delegates to must itself be @pure.
        (
            "@pure caller of stdlib lerp",
            "@pure\nfn lerp2(a: float, b: float, t: float) -> float { return lerp(a, b, t) }\n",
            Some("provably pure"),
        ),
    ];
    for (case, src, unwanted) in cases {
        for diags in both_views(src) {
            match unwanted {
                None => assert!(diags.is_empty(), "{case}: {diags:?}"),
                Some(u) => assert!(
                    !diags.iter().any(|d| d.message.contains(u)),
                    "{case}: unexpected {u:?} in {diags:?}"
                ),
            }
        }
    }
}

#[test]
fn update_publishes_lint_warnings() {
    let has = |diags: &[Diagnostic], msg: &str| diags.iter().any(|d| d.message.contains(msg));
    let diags = crate::fixture::published("fn f() -> int {\n  let dead = 1\n  2\n}\n");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("unused variable `dead`")
                && d.severity == Some(DiagnosticSeverity::WARNING)),
        "update() must return the unused_var warning for the edited file: {diags:?}"
    );

    let diags = crate::fixture::published(
        "@artifact_definition(catalog=\"widgets\", key_param=\"key\")\n\
         deco init widget(key: string) {}\n\
         fn f() { let dead = 1 }\n",
    );
    assert!(has(&diags, "unused variable `dead`"), "{diags:?}");
    assert!(
        !has(&diags, "unused variable `key`"),
        "artifact-definition metadata consumes `key`: {diags:?}"
    );
}

#[test]
fn broken_code_never_crashes_the_lsp() {
    // Every entry point must return best-effort on partial code; this fails by
    // panicking.
    let broken = [
        "fn f(",
        "fn f() {",
        "fn f() { let x = ",
        "struct S {",
        "enum E { A,",
        "fn f() { match ",
        "fn f(s: S) { s.",
        "fn f() { if x { ",
        "fn f() -> { }",
        "fn f() { for i in }",
        "fn f() { foo( }",
        "fn f() { [1, 2 }",
        "fn f() { _ + _ }",
        "fn f() { x.y.z.",
        "}}}",
    ];
    for src in broken {
        let mut bone = workspace(src);
        for line in 0..3 {
            for ch in 0..6 {
                let pos = Position::new(line, ch);
                let _ = bone.hover(PATH, pos);
                let _ = bone.completion(PATH, pos);
                let _ = bone.definition(PATH, pos);
                let _ = bone.references(PATH, pos);
            }
        }
        let _ = bone.diagnostics_for(PATH);
        let _ = bone.document_symbols(PATH);
        let _ = bone.inlay_hints(PATH, Range::new(Position::new(0, 0), Position::new(3, 0)));
    }
}

#[test]
fn edit_then_requery_updates_diagnostics() {
    let mut bone = workspace("fn f() -> int {\n  let x: int = \"bad\"\n  x\n}\n");
    assert!(!bone.diagnostics_for(PATH).is_empty(), "broken -> errors");
    bone.update(PATH, "fn f() -> int {\n  let x: int = 1\n  x\n}\n".into());
    let diags = bone.diagnostics_for(PATH);
    assert!(diags.is_empty(), "fixed -> clean: {diags:?}");
}

#[test]
fn project_lints() {
    let lib = "[package]\nname = \"l\"\ntype = \"lib\"\n";
    let bin = "[package]\nname = \"g\"\ntype = \"bin\"\n";
    let mentions = |d: &Diagnostic, needle: &str| {
        d.message.contains(needle) || d.code == Some(NumberOrString::String(needle.into()))
    };
    // (case, files, opened file, file whose diagnostics are checked,
    //  expected (message fragment, severity), unwanted message fragment or code)
    let cases: &[(
        &str,
        &[(&str, &str)],
        &str,
        &str,
        Option<(&str, DiagnosticSeverity)>,
        &[&str],
    )] = &[
        (
            "unused_fn runs on bin packages",
            &[
                ("stick.toml", bin),
                ("m.rock", "fn main() {}\nfn dead() {}\n"),
            ],
            "m.rock",
            "m.rock",
            Some(("unused function `dead`", DiagnosticSeverity::WARNING)),
            &[],
        ),
        // External consumers the project cannot see call a lib's API.
        (
            "unused_fn is silent for lib packages",
            &[("stick.toml", lib), ("m.rock", "fn public_api() {}\n")],
            "m.rock",
            "m.rock",
            None,
            &["unused function"],
        ),
        // A bin's session holds its whole compilation unit; without the report-set
        // filter, opening one game would lint a shared engine as if that game were
        // its only consumer. Reachability is the lint that differs this way.
        (
            "unused_fn is silent for a dependency of a bin",
            &[
                (
                    "engine/stick.toml",
                    "[package]\nname = \"engine\"\ntype = \"lib\"\n",
                ),
                ("engine/e.rock", "fn engine_helper() {}\n"),
                (
                    "game/stick.toml",
                    "[package]\nname = \"g\"\ntype = \"bin\"\n\n[dependencies]\nengine = { path = \"../engine\" }\n",
                ),
                ("game/m.rock", "fn main() {}\n"),
            ],
            "game/m.rock",
            "engine/e.rock",
            None,
            &["unused function"],
        ),
        // Error severity, matching the build gate, so a red squiggle predicts a
        // failed build. The lint resolves the real stdlib `cast`.
        (
            "transmute_of_nullable",
            &[
                ("stick.toml", "[package]\nname = \"g\"\n"),
                (
                    "m.rock",
                    "fn maybe() -> int? {\n  return 1\n}\nfn main() -> int {\n  let _x: int = cast(maybe())\n  3\n}\n",
                ),
            ],
            "m.rock",
            "m.rock",
            Some((
                "`cast` strips nullability from a `int?`",
                DiagnosticSeverity::ERROR,
            )),
            &[],
        ),
        // `backend_sibling_mismatch` is an explicit parity audit, not an LSP lint.
        (
            "mismatched backend siblings",
            &[
                ("stick.toml", lib),
                (
                    "m.rock",
                    "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: float) -> int = \"return x;\"\n",
                ),
            ],
            "m.rock",
            "m.rock",
            None,
            &["backend_sibling_mismatch", "backend sibling mismatch"],
        ),
        (
            "matching backend siblings",
            &[
                ("stick.toml", lib),
                (
                    "m.rock",
                    "inline(lua) fn probe(x: int) -> int = \"return x\"\ninline(js) fn probe(x: int) -> int = \"return x;\"\n",
                ),
            ],
            "m.rock",
            "m.rock",
            None,
            &["backend_sibling_mismatch", "backend sibling mismatch"],
        ),
    ];
    for (case, files, opened, checked, wanted, unwanted) in cases {
        let project = Project::new(files);
        let (_, per_file) = project.open(opened);
        let checked = project.path(checked);
        assert!(
            per_file.iter().any(|(p, _)| *p == checked),
            "{case}: nothing published for {checked}"
        );
        let diags = diagnostics_in(per_file, &checked);
        if let Some((msg, severity)) = wanted {
            assert!(
                diags
                    .iter()
                    .any(|d| d.message.contains(msg) && d.severity == Some(*severity)),
                "{case}: no {msg:?} at {severity:?} in {diags:?}"
            );
        }
        for u in *unwanted {
            assert!(
                !diags.iter().any(|d| mentions(d, u)),
                "{case}: unexpected {u:?} in {diags:?}"
            );
        }
    }
}

// Modules are baselined at load and re-stamped as they re-infer, so only the
// relative freshness proves the re-stamp ("ago is Some" passes off the baseline).
#[test]
fn edit_restamps_module_recency_below_sibling() {
    let project = Project::new(&[
        ("stick.toml", "[package]\nname = \"p\"\n"),
        ("a.rock", "fn a() -> int { 1 }\n"),
        ("b.rock", "fn b() -> int { 1 }\n"),
    ]);
    let (a, b) = (project.path("a.rock"), project.path("b.rock"));
    let mut bone = BoneWorkspace::new();
    bone.update(&a, "fn a() -> int { 2 }\n".to_string());
    std::thread::sleep(std::time::Duration::from_millis(25));
    bone.update(&b, "fn b() -> int { 2 }\n".to_string());
    let ago: std::collections::HashMap<String, Option<u64>> = bone
        .projects_summary()
        .into_iter()
        .flat_map(|(_, mods)| mods)
        .map(|m| (m.id, m.ago_ms))
        .collect();
    let ago_a = ago[&a].expect("a baselined at load");
    let ago_b = ago[&b].expect("b stamped on edit");
    assert!(ago_b < ago_a, "ago_b={ago_b} ago_a={ago_a}");
}

fn completion_labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    workspace(src)
        .completion(PATH, Position::new(line, ch))
        .into_iter()
        .map(|i| i.label)
        .collect()
}

#[test]
fn completion_offers_global_namespace_by_fuzzy_prefix() {
    let labels = completion_labels("fn main() {\n  vec\n}\n", 1, 5);
    assert_offers(&labels, &["vec_new"]);
    // Filtering is by case-insensitive subsequence, not strict prefix, but it
    // still filters.
    let subseq_ci = |name: &str| {
        let mut want = b"vec".iter().peekable();
        for c in name.bytes() {
            if want.peek() == Some(&&c.to_ascii_lowercase()) {
                want.next();
            }
        }
        want.peek().is_none()
    };
    let leaked: Vec<&String> = labels.iter().filter(|l| !subseq_ci(l)).collect();
    assert!(leaked.is_empty(), "not subsequences of `vec`: {leaked:?}");
}

#[test]
fn completion_cases() {
    // (case, source with cursor, label patterns; `!` forbids a label)
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "local declared after the cursor",
            "fn f() -> int {\n  l<|>\n  let later = 5\n  later\n}\n",
            &["!later"],
        ),
        (
            "field prefix filters members",
            "struct Player { position_x: float, health: float }\nfn foo(player: Player) {\n  player.pos<|>\n}\n",
            &["position_x: float", "!health: float"],
        ),
    ];
    for (case, marked, patterns) in cases {
        assert_labels(case, &labels_of(&complete_in(PATH, marked)), patterns);
    }
}

// The URI once was `file://unified.rock` (host `unified.rock`, empty path),
// and nvim crashed applying a stdlib line number to an empty buffer.
#[test]
fn definition_of_stdlib_type_resolves_to_a_real_file() {
    let bone = workspace("struct Mob { health: int }\nfn f(e: Entity<Mob>) {}\n");
    let loc = bone
        .definition(PATH, Position::new(1, 9))
        .expect("goto-def on a stdlib type should resolve");
    let uri = loc.uri.as_str();
    assert!(
        uri.starts_with("file:///") && uri.ends_with("unified.rock"),
        "stdlib def URI must be an absolute file path, got {uri:?}"
    );
    let text = std::fs::read_to_string(loc.uri.to_file_path().unwrap()).unwrap();
    let line = loc.range.start.line as usize;
    let target = text.lines().nth(line).unwrap_or("");
    assert!(target.contains("Entity"), "line {line}: {target:?}");
}

#[test]
fn signature_help_cases() {
    let draw_list = "struct DrawList { x: int }\n";
    let rect_outline = "fn rect_outline(dl: DrawList, pos: int, size: int) { }\n";
    // (case, source, cursor, first signature label, active parameter). In the
    // UFCS cases the receiver fills param 0, so the empty written argument list
    // sits on param 1.
    let cases = [
        (
            "second argument",
            "fn add(a: int, b: int) -> int {\n  a + b\n}\nfn main() {\n  let r = add(1, 2)\n}\n"
                .to_string(),
            Position::new(4, 17),
            "fn add(a: int, b: int) -> int",
            Some(1),
        ),
        (
            "UFCS receiver shifts the active param",
            format!(
                "{draw_list}{rect_outline}fn probe(screen: DrawList) {{\nscreen.rect_outline()\n}}\n"
            ),
            Position::new(3, 20),
            "fn rect_outline(dl: DrawList, pos: int, size: int)",
            Some(1),
        ),
        // The receiver's own closed parens must not be taken for the call's.
        (
            "UFCS receiver that is itself a call",
            format!(
                "{draw_list}fn get_screen() -> DrawList {{ DrawList {{ x: 0 }} }}\n{rect_outline}fn probe() {{\nget_screen().rect_outline()\n}}\n"
            ),
            Position::new(4, 26),
            "fn rect_outline(dl: DrawList, pos: int, size: int)",
            Some(1),
        ),
        (
            "param defaults render as optional",
            "fn plot(x: int, y: int = 0, label: string = \"pt\") {\n}\nfn main() {\n  plot(1)\n}\n"
                .to_string(),
            Position::new(3, 7),
            "fn plot(x: int, y: int = 0, label: string = \"pt\")",
            Some(0),
        ),
    ];
    for (case, src, pos, label, active) in cases {
        let sh = workspace(&src)
            .signature_help(PATH, pos)
            .unwrap_or_else(|| panic!("{case}: no signature help"));
        assert_eq!(sh.signatures[0].label, label, "{case}");
        assert_eq!(sh.active_parameter, active, "{case}");
    }
}

// The receiver type fills the first param slot even while the call is
// arity-incomplete.
#[test]
fn signature_help_filters_overloads_by_ufcs_receiver_type() {
    let src = "struct Ring<T> { data: [T] }\n\
               struct Bag<T> { stuff: [T] }\n\
               fn Ring<T>.add(r, x: T) { }\n\
               fn Bag<T>.add(b, y: T) { }\n\
               fn main() {\n\
                 let r: Ring<int> = Ring { data: [] }\n\
                 r.add()\n\
               }\n";
    let sh = workspace(src)
        .signature_help(PATH, Position::new(6, 6))
        .expect("signature help resolves the r.add(...) call");
    let labels: Vec<&str> = sh.signatures.iter().map(|s| s.label.as_str()).collect();
    assert!(labels.iter().any(|l| l.contains("Ring<T>")), "{labels:?}");
    assert!(!labels.iter().any(|l| l.contains("Bag<T>")), "{labels:?}");
}

fn reference_starts(bone: &BoneWorkspace, pos: Position) -> Vec<(u32, u32)> {
    let mut starts: Vec<(u32, u32)> = bone
        .references(PATH, pos)
        .iter()
        .map(|l| (l.range.start.line, l.range.start.character))
        .collect();
    starts.sort();
    starts
}

// Find-references is specific to the overload under the cursor; rename stays
// name-global (see rename_cases).
#[test]
fn references_on_an_overload_are_specific_to_that_overload() {
    let src = "struct A {}\nstruct B {}\nfn foo(a: A) -> int { 1 }\nfn foo(b: B) -> int { 2 }\nfn main() {\n  let x = A {}\n  let y = B {}\n  let r = foo(x) + foo(y)\n}\n";
    let starts = reference_starts(&workspace(src), Position::new(2, 3));
    assert!(starts.contains(&(2, 3)), "overload-1 decl: {starts:?}");
    assert!(starts.contains(&(7, 10)), "foo(x): {starts:?}");
    assert!(!starts.contains(&(3, 3)), "overload-2 decl: {starts:?}");
    assert!(!starts.contains(&(7, 19)), "foo(y): {starts:?}");
}

#[test]
fn references_from_a_named_arg_label_find_the_resolved_overloads_param() {
    let src = "fn put(count: int) {\n  print(count)\n}\nfn put(count: string) {\n  print(count)\n}\nfn main() {\n  put(count: 1)\n  put(count: \"a\")\n}\n";
    let bone = workspace(src);
    assert_eq!(
        reference_starts(&bone, Position::new(7, 7)),
        [(0, 7), (1, 8), (7, 6)]
    );
    assert_eq!(
        reference_starts(&bone, Position::new(8, 7)),
        [(3, 7), (4, 8), (8, 6)]
    );
    let range = bone
        .prepare_rename(PATH, Position::new(7, 7))
        .expect("no refusal")
        .expect("a label is renameable");
    assert_eq!(
        (range.start, range.end),
        (Position::new(7, 6), Position::new(7, 11))
    );
    assert_eq!(
        rename_lines(&bone, Position::new(7, 7), "amount", 5),
        [0, 1, 7]
    );
}

/// The sorted lines a rename edits, checking every edit writes `new_name` over
/// exactly `name_len` columns.
fn rename_lines(bone: &BoneWorkspace, pos: Position, new_name: &str, name_len: usize) -> Vec<u32> {
    let edit = bone
        .rename(PATH, pos, new_name)
        .expect("no refusal")
        .expect("rename");
    let mut lines: Vec<u32> = edit
        .changes
        .expect("changes")
        .into_values()
        .flatten()
        .map(|e| {
            assert_eq!(e.new_text, new_name, "every edit writes the new name");
            assert_eq!(
                e.range.end.character - e.range.start.character,
                name_len as u32,
                "every edit spans exactly the identifier"
            );
            e.range.start.line
        })
        .collect();
    lines.sort();
    lines
}

#[test]
fn rename_cases() {
    // (case, source, cursor, old name, edited lines)
    let cases: &[(&str, &str, (u32, u32), &str, &[u32])] = &[
        (
            "local decl and uses",
            "fn f() -> int {\n  let count = 1\n  let total = count + count\n  total\n}\n",
            (2, 14),
            "count",
            &[1, 2, 2],
        ),
        // A same-named field on another struct is untouched.
        (
            "field is type-scoped",
            "struct Person { name: string }\nstruct Company { name: string }\nfn main() {\n  let p = Person { name: \"a\" }\n  let c = Company { name: \"b\" }\n  print(p.name)\n  print(c.name)\n}\n",
            (0, 16),
            "name",
            &[0, 3, 5],
        ),
        (
            "struct decl, annotations and literal head",
            "struct Point { x: int }\nfn dist(p: Point) -> Point { p }\nfn main() {\n  let q: Point = Point { x: 1 }\n}\n",
            (0, 7),
            "Point",
            &[0, 1, 1, 3, 3],
        ),
        (
            "every overload and call",
            "fn p(x: int) -> int { x }\nfn p(x: float) -> float { x }\nfn main() {\n  let a = p(1)\n  let b = p(2.0)\n}\n",
            (0, 3),
            "p",
            &[0, 1, 3, 4],
        ),
    ];
    for (case, src, (line, ch), old, lines) in cases {
        let bone = workspace(src);
        let got = rename_lines(&bone, Position::new(*line, *ch), "renamed", old.len());
        assert_eq!(got, *lines, "{case}");
    }
}

// A newtype's generated `__op_add` shares its name span; resolving the decl as
// that function would rewrite every `__op_add` in the stdlib.
#[test]
fn rename_newtype_edits_decl_and_ctor_in_file_only() {
    let bone = workspace("newtype Health(int)\nfn main() {\n  let h = Health(5)\n}\n");
    let edit = bone
        .rename(PATH, Position::new(0, 9), "Hp")
        .expect("no refusal")
        .expect("newtype rename resolves");
    #[expect(
        clippy::mutable_key_type,
        reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
    )]
    let changes = edit.changes.expect("changes");
    assert_eq!(changes.len(), 1, "edits stay in the user file: {changes:?}");
    let lines = rename_lines(&bone, Position::new(0, 9), "Hp", "Health".len());
    assert_eq!(lines, vec![0, 2], "decl + ctor call");
}

#[test]
fn prepare_rename_agrees_with_rename_on_refusals_and_struct_generics() {
    let src = "inline(lua) fn probe(x: int) -> int = \"return x\"\nmacro double(v: expr) -> int { v + v }\nstruct Box<T> {\n  value: T,\n}\nfn main() {\n  print(probe(double(1)))\n}\n";
    let bone = workspace(src);
    let refusal = |pos| {
        let prepared = bone.prepare_rename(PATH, pos).unwrap_err();
        let renamed = bone.rename(PATH, pos, "renamed").unwrap_err();
        assert_eq!(prepared, renamed, "prepare and rename refuse alike");
        prepared.0
    };
    let param = refusal(Position::new(0, 21));
    assert!(param.contains("inline(lua) body of `probe`"), "{param}");
    let mac = refusal(Position::new(6, 15));
    assert!(mac.contains("cannot rename a macro"), "{mac}");
    // A struct generic renames from its declaration like from any use.
    let range = bone
        .prepare_rename(PATH, Position::new(2, 11))
        .expect("allowed")
        .expect("a struct generic is renameable");
    assert_eq!((range.start.line, range.start.character), (2, 11));
    assert_eq!(rename_lines(&bone, Position::new(2, 11), "U", 1), [2, 3]);
}

#[test]
fn prepare_rename_ranges_just_the_name() {
    let load = "fn load_image_asset(p: string) -> int {\n  return 1\n}\nfn main() {\n  let img = load_image_asset(\"x\")\n}\n";
    // (case, source, cursor, expected range). A declaration site has no
    // `definition`, and a call site must not range the whole call expression.
    let cases = [
        ("function declaration", load, (0, 10), ((0, 3), (0, 19))),
        ("call site", load, (4, 14), ((4, 12), (4, 28))),
        (
            "local binding",
            "fn f() -> int {\n  let count = 1\n  count\n}\n",
            (1, 7),
            ((1, 6), (1, 11)),
        ),
    ];
    for (case, src, (line, ch), ((sl, sc), (el, ec))) in cases {
        let range = workspace(src)
            .prepare_rename(PATH, Position::new(line, ch))
            .expect("no refusal")
            .unwrap_or_else(|| panic!("{case}: nothing to rename"));
        assert_eq!(
            (range.start, range.end),
            (Position::new(sl, sc), Position::new(el, ec)),
            "{case}"
        );
    }
}

#[test]
fn document_symbols_list_top_level_items_with_kinds() {
    let src = "struct Point { x: int }\nenum Color { Red }\nconst LIMIT: int = 3\nglobal MAX: int = 9\nfn main() {\n  let q = 1\n}\n";
    let syms = workspace(src).document_symbols(PATH);
    let kind_of = |name: &str| syms.iter().find(|s| s.name == name).map(|s| s.kind);
    use lsp_types::SymbolKind;
    assert_eq!(kind_of("Point"), Some(SymbolKind::STRUCT));
    assert_eq!(kind_of("Color"), Some(SymbolKind::ENUM));
    assert_eq!(kind_of("LIMIT"), Some(SymbolKind::CONSTANT));
    assert_eq!(kind_of("MAX"), Some(SymbolKind::VARIABLE));
    assert_eq!(kind_of("main"), Some(SymbolKind::FUNCTION));
    assert_eq!(kind_of("q"), None, "locals are not document symbols");
    let main = syms.iter().find(|s| s.name == "main").unwrap();
    assert!(main.range.start.line <= 4 && 4 <= main.range.end.line);
}

fn inlay_labels(src: &str, range: Range) -> Vec<String> {
    workspace(src)
        .inlay_hints(PATH, range)
        .into_iter()
        .map(|h| match h.label {
            lsp_types::InlayHintLabel::String(s) => s,
            _ => panic!("expected a string inlay label"),
        })
        .collect()
}

#[test]
fn inlay_hint_cases() {
    let lines = |a, b| Range::new(Position::new(a, 0), Position::new(b, 0));
    // (case, source, range, labels). A named argument already shows its name;
    // mapping it positionally once labeled `d: 4` as `c:`.
    let cases = [
        (
            "only the unannotated let",
            "fn main() {\n  let n = 1\n  let s: int = 2\n}\n",
            lines(0, 4),
            vec![": int"],
        ),
        (
            "the requested range only",
            "fn main() {\n  let n = 1\n  let m = 2\n}\n",
            lines(1, 2),
            vec![": int"],
        ),
        (
            "only positional call args",
            "fn make(a: int, b: int, c: int, d: int) -> int { return a }\nfn r() -> int {\n    return make(1, b: 2, d: 4)\n}\n",
            lines(0, 4),
            vec!["a:"],
        ),
    ];
    for (case, src, range, want) in cases {
        assert_eq!(inlay_labels(src, range), want, "{case}");
    }
}

#[test]
fn unknown_file_degrades_to_empty() {
    let bone = BoneWorkspace::new();
    assert!(
        bone.hover("/tmp/never_opened.rock", Position::new(0, 0))
            .is_none()
    );
    assert!(bone.diagnostics_for("/tmp/never_opened.rock").is_empty());
}

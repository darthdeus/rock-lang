//! End-to-end tests for the bone-backed LSP facade (`bone_query`).
//!
//! These exercise the facade directly (no LSP wire protocol): feed a file in,
//! query it, assert the LSP-shaped result. Hover + diagnostics are real; the
//! rest are gap stubs that must return empty (never panic) until bone grows the
//! query API. See `docs/lsp-bone-migration-plan.md`.

use lsp_types::Position;
use bone_query::BoneWorkspace;

const PATH: &str = "/tmp/rock_lsp_bone_test_main.rock";

fn fresh(src: &str) -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone
}

#[test]
fn hover_reports_inferred_type() {
    // `let x = 1` -> hovering the binding `x` (line 1, char 6) yields its type.
    let src = "fn main() {\n  let x = 1\n}\n";
    let bone = fresh(src);
    let hover = bone.hover(PATH, Position::new(1, 6));
    let hover = hover.expect("hover should resolve on the `x` binding");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(
        text.to_lowercase().contains("int"),
        "hover for an int binding should mention int, got: {text:?}"
    );
}

#[test]
fn hover_on_type_hole_reports_hole_and_scope() {
    // Hovering the `?` in `let x: ? = count + 1` (line 2, char 9) yields the
    // typed-hole report (kind label + inferred type + in-scope bindings) -- the
    // same info `rock-lsp hole-info` prints, not a bare `Type: int`.
    let src = "fn main() {\n  let count = 5\n  let x: ? = count + 1\n}\n";
    let bone = fresh(src);
    let hover = bone
        .hover(PATH, Position::new(2, 9))
        .expect("hover on the `?` type hole should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(
        text.contains("Type hole"),
        "hover on a hole should label it a hole, got: {text:?}"
    );
    assert!(
        text.contains("count: int"),
        "hover on a hole should dump the in-scope bindings, got: {text:?}"
    );
}

#[test]
fn hover_on_expression_hole_reports_hole_and_scope() {
    // Hovering the `?` in `let x: int = ?` (line 2, char 15) yields the
    // expression-hole report (expected type + in-scope bindings).
    let src = "fn main() {\n  let a = 1\n  let x: int = ?\n}\n";
    let bone = fresh(src);
    let hover = bone
        .hover(PATH, Position::new(2, 15))
        .expect("hover on the `?` expression hole should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(
        text.contains("Expression hole expecting: int"),
        "hover on an expr hole should report the expected type, got: {text:?}"
    );
    assert!(
        text.contains("a: int"),
        "hover on a hole should dump the in-scope bindings, got: {text:?}"
    );
}

#[test]
fn hover_on_a_global_declaration_reports_its_type() {
    // `global score: int` -- hovering the declared name `score` should report
    // its type, like hovering a use of it does. (Item-level decls, not body
    // expressions, so hover_at needs an item fallback for globals.)
    let src = "global score: int\nfn main() {}\n";
    let bone = fresh(src);
    let hover = bone
        .hover(PATH, Position::new(0, 9))
        .expect("hover on the global declaration name should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("int"), "global decl hover names its type: {text:?}");
}

#[test]
fn hover_on_a_type_annotation_shows_the_definition() {
    // `let m: Mob` -- hovering the type `Mob` shows the struct definition, like
    // a struct-literal head does. (Body-level type annotations are in the body's
    // node_index; the LocalTy/StaticTy arm renders them.)
    let src = "struct Mob { hp: int }\nfn main() {\n  let m: Mob = Mob { hp: 1 }\n}\n";
    let bone = fresh(src);
    let hover = bone
        .hover(PATH, Position::new(2, 10))
        .expect("hover on the `Mob` type annotation should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("struct Mob"), "shows the struct def: {text:?}");
    assert!(text.contains("hp: int"), "shows the field: {text:?}");
}

#[test]
fn hover_on_signature_types_shows_the_definition() {
    // Param and return type annotations (`fn f(x: Mob) -> Mob`) both hover as
    // the struct definition. bone is consistent (full def for every
    // type-annotation position); rock shows the full def only for params, but
    // bone's consistent form is the intended behavior.
    let src = "struct Mob { hp: int }\nfn use_it(x: Mob) -> Mob {\n  x\n}\n";
    let bone = fresh(src);
    // line 1: `fn use_it(x: Mob) -> Mob {` -- param `Mob` at col 13, return at col 21.
    let hover_text = |pos| match bone.hover(PATH, pos).map(|h| h.contents) {
        Some(lsp_types::HoverContents::Markup(m)) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    let param = hover_text(Position::new(1, 14));
    assert!(param.contains("struct Mob") && param.contains("hp: int"), "param type: {param:?}");
    let ret = hover_text(Position::new(1, 22));
    assert!(ret.contains("struct Mob") && ret.contains("hp: int"), "return type: {ret:?}");
}

#[test]
fn hover_on_a_struct_field_declaration_shows_field_of() {
    let src = "struct Vec2 { x: float, y: float }\nstruct Mob {\n  pos: Vec2,\n  dir: int,\n}\n";
    let bone = fresh(src);
    // `pos` on line 2 at col 2.
    let hover = bone
        .hover(PATH, Position::new(2, 3))
        .expect("hover on a struct field decl name should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("pos: Vec2 (field of Mob)"), "field-of hover: {text:?}");
}

#[test]
fn hover_on_a_newtype_declaration_names_it_not_an_op() {
    // Regression: hovering a newtype decl name resolved to its auto-generated
    // `__op_add` (whose name_span is the newtype name). It must show the newtype.
    let src = "newtype Health(int)\nfn main() {}\n";
    let bone = fresh(src);
    let hover = bone
        .hover(PATH, Position::new(0, 9))
        .expect("hover on the newtype decl name should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("newtype Health(int)"), "newtype hover: {text:?}");
    assert!(!text.contains("__op"), "must not resolve to the auto-gen op: {text:?}");
}

#[test]
fn hover_on_a_struct_record_pattern_head_shows_the_struct() {
    let src = "struct Mob { hp: int }\nfn f(m: Mob) {\n  match m {\n    Mob { hp: _h } => {}\n  }\n}\n";
    let bone = fresh(src);
    // `Mob` head on line 3 at col 4.
    let hover = bone
        .hover(PATH, Position::new(3, 5))
        .expect("hover on a struct record-pattern head should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("struct Mob"), "record-pat head -> struct: {text:?}");
}

#[test]
fn hover_on_an_enum_variant_pattern_names_the_enum() {
    // Hovering `MobKind::Skeleton` in a match arm resolves to the enum, NOT the
    // match's result type ("string"). The match expr is no longer a catch-all;
    // the variant pattern is resolved from the raw AST.
    let src = "enum MobKind { Skeleton, Zombie }\nfn name(k: MobKind) -> string {\n  match k {\n    MobKind::Skeleton => \"s\"\n    MobKind::Zombie => \"z\"\n  }\n}\n";
    let bone = fresh(src);
    let text = |pos| match bone.hover(PATH, pos).map(|h| h.contents) {
        Some(lsp_types::HoverContents::Markup(m)) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    // line 3: `    MobKind::Skeleton => "s"` -- qualifier col 4, variant col 13.
    let variant = text(Position::new(3, 16));
    assert!(variant.contains("enum MobKind"), "variant pattern -> enum: {variant:?}");
    assert!(!variant.contains("string"), "must not be the match result type: {variant:?}");
    let qualifier = text(Position::new(3, 5));
    assert!(qualifier.contains("enum MobKind"), "enum qualifier -> enum: {qualifier:?}");
}

#[test]
fn hover_on_a_global_type_annotation_shows_the_definition() {
    // `global boss: Mob` -- hovering the type `Mob` shows the struct def. A
    // value-less global has no inference body, so this resolves at item level.
    let src = "struct Mob { hp: int }\nglobal boss: Mob\nfn main() {}\n";
    let bone = fresh(src);
    // `Mob` in `global boss: Mob` is at col 13 on line 1.
    let hover = bone
        .hover(PATH, Position::new(1, 14))
        .expect("hover on the global's type annotation should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("struct Mob"), "shows the struct def: {text:?}");
    assert!(text.contains("hp: int"), "shows the field: {text:?}");
}

#[test]
fn hover_on_an_enum_declaration_names_it() {
    let src = "enum Color { Red, Green }\nfn main() {}\n";
    let bone = fresh(src);
    let hover = bone
        .hover(PATH, Position::new(0, 6))
        .expect("hover on the enum declaration name should resolve");
    let text = match hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    };
    assert!(text.contains("Color"), "enum decl hover names the enum: {text:?}");
}

#[test]
fn clean_file_has_no_diagnostics() {
    let src = "fn main() {\n  let x = 1\n}\n";
    let bone = fresh(src);
    assert!(
        bone.diagnostics_for(PATH).is_empty(),
        "a well-typed file must have no diagnostics: {:?}",
        bone.diagnostics_for(PATH)
    );
}

#[test]
fn type_error_is_reported() {
    // Assigning a string to an int-annotated binding must fail to type-check.
    let src = "fn main() {\n  let x: int = \"hello\"\n}\n";
    let bone = fresh(src);
    let diags = bone.diagnostics_for(PATH);
    assert!(
        !diags.is_empty(),
        "type error must surface a diagnostic, got none"
    );
    assert_eq!(diags[0].source.as_deref(), Some("bone"));
}

#[test]
fn non_exhaustive_match_is_an_error() {
    // `match e {}` over a 3-variant enum is non-exhaustive -- rock rejects this
    // ("Non-exhaustive match expression. Missing patterns: ..."); bone's LSP must
    // surface the same, listing the uncovered variants in declaration order.
    let src = "\
enum E { A, B, C }

fn f(e: E) {
    match e {
        E::A => {}
    }
}
";
    let bone = fresh(src);
    let diags = bone.diagnostics_for(PATH);
    let d = diags
        .iter()
        .find(|d| d.message.contains("Non-exhaustive"))
        .unwrap_or_else(|| panic!("expected a non-exhaustive diagnostic, got: {diags:?}"));
    assert_eq!(d.severity, Some(lsp_types::DiagnosticSeverity::ERROR));
    assert_eq!(d.source.as_deref(), Some("bone"));
    assert!(
        d.message.contains("Missing patterns: B, C"),
        "msg should list the uncovered variants in order: {}",
        d.message
    );
}

#[test]
fn empty_match_reports_all_variants_missing() {
    let src = "enum E { A, B, C }\n\nfn f(e: E) {\n    match e {\n    }\n}\n";
    let bone = fresh(src);
    let diags = bone.diagnostics_for(PATH);
    assert!(
        diags.iter().any(|d| d.message.contains("Missing patterns: A, B, C")),
        "empty match misses every variant: {diags:?}"
    );
}

#[test]
fn exhaustive_and_catch_all_matches_have_no_diagnostic() {
    let exhaustive = "enum E { A, B }\n\nfn f(e: E) {\n    match e {\n        E::A => {}\n        E::B => {}\n    }\n}\n";
    let catch_all = "enum E { A, B }\n\nfn f(e: E) {\n    match e {\n        E::A => {}\n        _ => {}\n    }\n}\n";
    for src in [exhaustive, catch_all] {
        let bone = fresh(src);
        assert!(
            !bone.diagnostics_for(PATH).iter().any(|d| d.message.contains("Non-exhaustive")),
            "covered match must not warn: {src:?} -> {:?}",
            bone.diagnostics_for(PATH)
        );
    }
}

#[test]
fn guarded_variant_arm_does_not_cover_its_variant() {
    // rock parity: `E::A if c =>` may not fire, so A is still missing. This needs
    // the skull-parser guard fix (`docs/match-guard-parse-ambiguity.md`) -- before
    // it, a guarded arm collapsed the whole match at parse time.
    let src = "enum E { A, B }\n\nfn f(e: E, x: bool) {\n    match e {\n        E::A if x => {}\n        E::B => {}\n    }\n}\n";
    let bone = fresh(src);
    assert!(
        bone.diagnostics_for(PATH).iter().any(|d| d.message.contains("Missing patterns: A")),
        "guarded A is not covered: {:?}",
        bone.diagnostics_for(PATH)
    );
}

#[test]
fn unfilled_expression_hole_is_a_warning() {
    // `let y: int = ?` -- the `?` (an expression hole) should warn, ranged on
    // the `?`, naming the expected type. This is what makes the `?` our code
    // actions insert a visible, typed TODO instead of a silent placeholder.
    let src = "fn main() {\n  let y: int = ?\n}\n";
    let bone = fresh(src);
    let diags = bone.diagnostics_for(PATH);
    let hole = diags
        .iter()
        .find(|d| d.message.contains("expression hole"))
        .unwrap_or_else(|| panic!("expr hole should warn, got: {diags:?}"));
    assert_eq!(hole.severity, Some(lsp_types::DiagnosticSeverity::WARNING));
    assert!(hole.message.contains("expecting int"), "msg: {}", hole.message);
    assert_eq!(hole.source.as_deref(), Some("bone"));
    // `?` is at line 1, char 15.
    assert_eq!(hole.range.start, Position::new(1, 15), "ranged on the `?`");
}

#[test]
fn resolved_type_hole_is_a_hint() {
    // `let x: ? = 5` -- the type hole resolves to int; that's informational
    // (a hint), not a problem.
    let src = "fn main() {\n  let x: ? = 5\n}\n";
    let bone = fresh(src);
    let diags = bone.diagnostics_for(PATH);
    let hole = diags
        .iter()
        .find(|d| d.message.contains("type hole"))
        .unwrap_or_else(|| panic!("type hole should hint, got: {diags:?}"));
    assert_eq!(hole.severity, Some(lsp_types::DiagnosticSeverity::HINT));
    assert!(hole.message.contains("inferred as int"), "msg: {}", hole.message);
}

#[test]
fn filling_a_hole_clears_its_warning() {
    // A program with the hole filled has no hole diagnostic -- so the squiggle
    // disappears the moment the user replaces the `?`.
    let src = "fn main() {\n  let y: int = 42\n}\n";
    let bone = fresh(src);
    assert!(
        !bone
            .diagnostics_for(PATH)
            .iter()
            .any(|d| d.message.contains("hole")),
        "no hole diagnostic once filled"
    );
}

#[test]
fn hole_warnings_reach_the_interactive_update_path() {
    // The editor publishes diagnostics from `update`'s return value, so the
    // hole warning must show up there too (not only in `diagnostics_for`).
    let src = "fn main() {\n  let y: int = ?\n}\n";
    let mut bone = BoneWorkspace::new();
    let per_file = bone.update(PATH, src.to_string());
    let diags: Vec<_> = per_file
        .into_iter()
        .find(|(p, _)| p == PATH)
        .map(|(_, d)| d)
        .unwrap_or_default();
    assert!(
        diags.iter().any(|d| d.message.contains("expression hole")),
        "update path surfaces the hole warning: {diags:?}"
    );
}

#[test]
fn non_exhaustive_match_reaches_the_interactive_update_path() {
    // The editor publishes from `update`'s return value, so the non-exhaustive
    // error must surface there too (not only in `diagnostics_for`).
    let src = "enum E { A, B }\n\nfn f(e: E) {\n    match e {\n        E::A => {}\n    }\n}\n";
    let mut bone = BoneWorkspace::new();
    let per_file = bone.update(PATH, src.to_string());
    let diags: Vec<_> = per_file
        .into_iter()
        .find(|(p, _)| p == PATH)
        .map(|(_, d)| d)
        .unwrap_or_default();
    assert!(
        diags.iter().any(|d| d.message.contains("Non-exhaustive match")),
        "update path surfaces the non-exhaustive error: {diags:?}"
    );
}

#[test]
fn broken_code_never_crashes_the_lsp() {
    // The parse is total, so partial/broken code flows into lower+infer+query.
    // Every LSP entry point must return (best-effort), never panic -- this test
    // fails by panicking if any does. Companion to the broader mutation fuzz.
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
    for (i, src) in broken.iter().enumerate() {
        let mut bone = BoneWorkspace::new();
        let _ = bone.update(PATH, src.to_string());
        // Exercise every query at a spread of positions on the broken buffer.
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
        let _ = bone.inlay_hints(
            PATH,
            lsp_types::Range::new(Position::new(0, 0), Position::new(3, 0)),
        );
        // Reaching here for every snippet means nothing panicked.
        assert!(i < broken.len());
    }
}

#[test]
fn stdlib_using_program_type_checks() {
    // Proves warm stdlib is loaded into the session.
    let src = "fn main() {\n  let xs = vec_new()\n  xs.push(1)\n}\n";
    let bone = fresh(src);
    assert!(
        bone.diagnostics_for(PATH).is_empty(),
        "stdlib-using program should check clean: {:?}",
        bone.diagnostics_for(PATH)
    );
}

#[test]
fn edit_then_requery_updates_diagnostics() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, "fn main() {\n  let x: int = \"bad\"\n}\n".to_string());
    assert!(!bone.diagnostics_for(PATH).is_empty(), "broken edit -> errors");

    bone.update(PATH, "fn main() {\n  let x: int = 1\n}\n".to_string());
    assert!(
        bone.diagnostics_for(PATH).is_empty(),
        "fixed edit -> clean: {:?}",
        bone.diagnostics_for(PATH)
    );
}

#[test]
fn completion_offers_global_namespace_by_prefix() {
    // Cursor after `vec` -> the stdlib's vec_* functions surface.
    let src = "fn main() {\n  vec\n}\n";
    let mut bone = fresh(src);
    let items = bone.completion(PATH, Position::new(1, 5));
    assert!(
        items.iter().any(|c| c.label == "vec_new"),
        "expected vec_new among completions, got: {:?}",
        items.iter().map(|c| &c.label).take(20).collect::<Vec<_>>()
    );
    // The query filters by case-insensitive SUBSEQUENCE (fuzzy, not strict
    // prefix -- so `vc` would also find `vec_*`), but it still filters: every
    // item contains v-e-c in order, not the whole namespace.
    fn subseq_ci(name: &str, q: &[u8]) -> bool {
        let mut i = 0;
        for &c in name.as_bytes() {
            if i < q.len() && c.to_ascii_lowercase() == q[i] {
                i += 1;
            }
        }
        i == q.len()
    }
    assert!(
        items.iter().all(|c| subseq_ci(&c.label, b"vec")),
        "every completion is a subsequence match of `vec`; leaked: {:?}",
        items.iter().map(|c| &c.label).filter(|l| !subseq_ci(l, b"vec")).collect::<Vec<_>>()
    );
}

#[test]
fn definition_jumps_to_called_user_function() {
    // Cursor inside the `helper()` call -> declaration of `helper` (line 0).
    let src = "fn helper() -> int {\n  42\n}\nfn main() {\n  let x = helper()\n}\n";
    let bone = fresh(src);
    // line 4 "  let x = helper()" -> `helper` starts at char 10.
    let loc = bone
        .definition(PATH, Position::new(4, 12))
        .expect("definition should resolve the helper() call");
    assert_eq!(
        loc.range.start.line, 0,
        "helper is declared on line 0, got {:?}",
        loc.range
    );
}

#[test]
fn signature_help_renders_callee_and_active_param() {
    let src = "fn add(a: int, b: int) -> int {\n  a + b\n}\nfn main() {\n  let r = add(1, 2)\n}\n";
    let bone = fresh(src);
    // line 4 "  let r = add(1, 2)": cursor in the second argument (after the comma).
    let sh = bone
        .signature_help(PATH, Position::new(4, 17))
        .expect("signature help should resolve the add(...) call");
    assert_eq!(sh.signatures[0].label, "fn add(a: int, b: int) -> int");
    assert_eq!(sh.active_parameter, Some(1), "cursor is in the 2nd argument");
}

#[test]
fn definition_jumps_to_local_binding() {
    // Via bone_query::query::definition_at: a local-variable use resolves to its decl.
    let src = "fn f() -> int {\n  let count = 1\n  return count\n}\n";
    let bone = fresh(src);
    // line 2 "  return count" -> cursor inside `count`.
    let loc = bone
        .definition(PATH, Position::new(2, 10))
        .expect("definition should resolve the local `count`");
    assert_eq!(loc.range.start.line, 1, "count is declared on line 1");
}

#[test]
fn references_finds_local_decl_and_uses() {
    let src = "fn f() -> int {\n  let count = 1\n  let total = count + count\n  total\n}\n";
    let bone = fresh(src);
    // cursor on the first use of `count` (line 2, char 14).
    let locs = bone.references(PATH, Position::new(2, 14));
    let starts: Vec<(u32, u32)> = locs
        .iter()
        .map(|l| (l.range.start.line, l.range.start.character))
        .collect();
    // declaration (1,6) + two uses (2,14) and (2,22).
    assert_eq!(locs.len(), 3, "got {starts:?}");
    for expected in [(1, 6), (2, 14), (2, 22)] {
        assert!(starts.contains(&expected), "missing {expected:?} in {starts:?}");
    }
}

#[test]
fn rename_rewrites_local_decl_and_uses_precisely() {
    let src = "fn f() -> int {\n  let count = 1\n  let total = count + count\n  total\n}\n";
    let bone = fresh(src);
    let edit = bone
        .rename(PATH, Position::new(2, 14), "n")
        .expect("rename a local");
    let edits = edit.changes.expect("changes").into_values().next().unwrap();
    assert_eq!(edits.len(), 3, "decl + two uses");
    // every edit replaces a name-sized span with the new name (rename-safe).
    for e in &edits {
        assert_eq!(e.new_text, "n");
        assert_eq!(
            e.range.end.character - e.range.start.character,
            "count".len() as u32,
            "edit should span exactly the identifier, not the whole statement"
        );
    }
}

fn rename_lines(bone: &BoneWorkspace, pos: Position, new_name: &str, name_len: usize) -> Vec<u32> {
    let edit = bone.rename(PATH, pos, new_name).expect("rename");
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
                "every edit spans exactly the identifier (rename-safe)"
            );
            e.range.start.line
        })
        .collect();
    lines.sort();
    lines
}

#[test]
fn rename_field_is_type_scoped() {
    // Two structs share field `name`; renaming Person.name must NOT touch
    // Company.name -- a same-named field on a different struct.
    let src = "\
struct Person { name: string }
struct Company { name: string }
fn main() {
  let p = Person { name: \"a\" }
  let c = Company { name: \"b\" }
  print(p.name)
  print(c.name)
}
";
    let bone = fresh(src);
    // Cursor on Person's `name` field declaration (line 0).
    let lines = rename_lines(&bone, Position::new(0, 16), "fullName", "name".len());
    // Person decl (0), Person literal field (3), `p.name` access (5) -- and
    // none of Company's lines 1/4/6.
    assert_eq!(lines, vec![0, 3, 5], "only Person.name sites, not Company.name");
}

#[test]
fn rename_struct_hits_decl_annotations_and_literals() {
    let src = "\
struct Point { x: int }
fn dist(p: Point) -> Point { p }
fn main() {
  let q: Point = Point { x: 1 }
}
";
    let bone = fresh(src);
    // Cursor on the struct decl name `Point` (line 0, char 7).
    let lines = rename_lines(&bone, Position::new(0, 7), "P2", "Point".len());
    // decl (0), param + return annotations (1, 1), let annotation + literal (3, 3).
    assert_eq!(lines, vec![0, 1, 1, 3, 3], "decl + annotations + literal head");
}

#[test]
fn rename_overloaded_function_hits_all_overloads_and_calls() {
    let src = "\
fn p(x: int) -> int { x }
fn p(x: float) -> float { x }
fn main() {
  let a = p(1)
  let b = p(2.0)
}
";
    let bone = fresh(src);
    // Cursor on the first overload's name `p` (line 0, char 3).
    let lines = rename_lines(&bone, Position::new(0, 3), "q", "p".len());
    // Both decls (0, 1) and both calls (3, 4).
    assert_eq!(lines, vec![0, 1, 3, 4], "all overloads + all calls");
}

#[test]
fn rename_newtype_refuses_rather_than_corrupting_stdlib() {
    // Regression (DESTRUCTIVE): a newtype's auto-generated `__op_add` shares the
    // newtype's name_span, so `function_target` resolved the decl name to it and
    // `function_references_by_name("__op_add")` matched every `__op_add` across
    // the warm stdlib -- 80+ edits rewriting json.rock / unified.rock. The
    // `__`-op skip in `function_target` neutralizes that, so the decl now
    // resolves to no sites and rename refuses.
    //
    // SENTINEL: real newtype rename is pending the enum/variant HIR-span work
    // (docs/lsp-rename-coverage.md). When it lands this returns Some -- flip to
    // assert the in-file edit sites (decl + `Health(..)` ctor) and that nothing
    // leaks outside the user file.
    let src = "newtype Health(int)\nfn main() {\n  let h = Health(5)\n}\n";
    let bone = fresh(src);
    assert!(
        bone.rename(PATH, Position::new(0, 9), "Hp").is_none(),
        "newtype rename now resolves -- implement full support and flip this sentinel"
    );
}

#[test]
fn prepare_rename_on_a_function_definition_name_returns_the_name_range() {
    // F2 on the `fn load_image_asset` declaration name must offer a rename
    // ranged on the identifier -- not report "nothing to rename". (Regression:
    // prepare_rename gated on `gaps::definition`, which is None at a decl site.)
    let src = "fn load_image_asset(p: string) -> int {\n  return 1\n}\nfn main() {\n  let img = load_image_asset(\"x\")\n}\n";
    let bone = fresh(src);
    // `load_image_asset` on line 0 spans chars 3..19.
    let range = bone
        .prepare_rename(PATH, Position::new(0, 10))
        .expect("prepare_rename on a function decl name must offer a rename");
    assert_eq!(range.start, Position::new(0, 3), "range starts at the name");
    assert_eq!(range.end, Position::new(0, 19), "range ends at the name (16 chars)");
}

#[test]
fn prepare_rename_on_a_call_site_ranges_just_the_name_not_the_whole_call() {
    // F2 on a callee must prefill only the function name -- not the entire call
    // expression `load_image_asset("x")`. (Regression: prepare_rename used
    // `type_at`, which returns the enclosing Call expr span.)
    let src = "fn load_image_asset(p: string) -> int {\n  return 1\n}\nfn main() {\n  let img = load_image_asset(\"x\")\n}\n";
    let bone = fresh(src);
    // line 4 `  let img = load_image_asset("x")`: the callee name spans 12..28;
    // the whole call runs to char 33.
    let range = bone
        .prepare_rename(PATH, Position::new(4, 14))
        .expect("prepare_rename on a call site must offer a rename");
    assert_eq!(range.start, Position::new(4, 12), "range starts at the name");
    assert_eq!(
        range.end,
        Position::new(4, 28),
        "range ends at the name, not the whole call expression"
    );
}

#[test]
fn prepare_rename_on_a_local_binding_ranges_the_identifier() {
    // A local still prepares correctly (regression guard for the unified path).
    let src = "fn f() -> int {\n  let count = 1\n  count\n}\n";
    let bone = fresh(src);
    let range = bone
        .prepare_rename(PATH, Position::new(1, 7))
        .expect("prepare_rename on a local binding");
    assert_eq!(range.start, Position::new(1, 6));
    assert_eq!(range.end, Position::new(1, 11), "spans `count` (5 chars)");
}

#[test]
fn document_symbols_lists_top_level_items() {
    let src = "struct Point { x: int }\nenum Color { Red }\nglobal MAX: int = 9\nfn main() {\n  let q = 1\n}\n";
    let bone = fresh(src);
    let syms = bone.document_symbols(PATH);
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();

    for expected in ["Point", "Color", "MAX", "main"] {
        assert!(names.contains(&expected), "document symbols missing {expected:?}: {names:?}");
    }
    // Locals are NOT top-level symbols.
    assert!(!names.contains(&"q"), "locals must not appear as document symbols: {names:?}");

    // The `main` symbol's range must cover its declaration line (line 3).
    let main = syms.iter().find(|s| s.name == "main").unwrap();
    assert_eq!(main.kind, lsp_types::SymbolKind::FUNCTION);
    assert!(main.range.start.line <= 3 && 3 <= main.range.end.line);
}

#[test]
fn inlay_hints_annotate_unannotated_let_bindings() {
    // `let n = 1` has no type annotation -> a `: int` hint; `let s: int = 2` is
    // already annotated -> no hint.
    let src = "fn main() {\n  let n = 1\n  let s: int = 2\n}\n";
    let bone = fresh(src);
    let hints = bone.inlay_hints(PATH, lsp_types::Range::new(Position::new(0, 0), Position::new(4, 0)));

    let labels: Vec<String> = hints
        .iter()
        .map(|h| match &h.label {
            lsp_types::InlayHintLabel::String(s) => s.clone(),
            other => panic!("expected string label, got {other:?}"),
        })
        .collect();

    assert_eq!(hints.len(), 1, "exactly one unannotated binding (`n`), got {labels:?}");
    assert!(labels[0].contains("int"), "hint should render the inferred type, got {labels:?}");
}

#[test]
fn inlay_hints_respect_the_requested_range() {
    // A range covering only line 1 must not surface the binding on line 2.
    let src = "fn main() {\n  let n = 1\n  let m = 2\n}\n";
    let bone = fresh(src);
    let only_line1 = bone.inlay_hints(PATH, lsp_types::Range::new(Position::new(1, 0), Position::new(2, 0)));
    assert_eq!(only_line1.len(), 1, "range over line 1 should yield exactly the `n` hint");
}

#[test]
fn completion_includes_in_scope_locals_and_params() {
    // body of `f`: param `count`, local `total` (declared line 1). Completing a
    // fresh `t` on line 2 -> the local `total` is offered.
    let src = "fn f(count: int) -> int {\n  let total = 5\n  t\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(2, 3))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "total"), "local `total` (prefix `t`): {labels:?}");

    // param `count` is in scope throughout the body -> offered on prefix `c`.
    let src2 = "fn f(count: int) -> int {\n  let total = 5\n  c\n}\n";
    let mut bone2 = BoneWorkspace::new();
    bone2.update(PATH, src2.to_string());
    let labels2: Vec<String> = bone2
        .completion(PATH, Position::new(2, 3))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels2.iter().any(|l| l == "count"), "param `count` (prefix `c`): {labels2:?}");
}

#[test]
fn completion_excludes_locals_declared_after_the_cursor() {
    // Completing on line 1 (`l`) must not offer `later`, declared on line 2.
    let src = "fn f() -> int {\n  l\n  let later = 5\n  later\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(1, 3))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(!labels.iter().any(|l| l == "later"), "`later` is declared after the cursor: {labels:?}");
}

#[test]
fn completion_offers_struct_fields_on_broken_dot() {
    // The trigger `.` is a syntax error (breaks the body); member completion
    // must still resolve `p`'s struct fields. param receiver.
    let src = "struct Point { x: int, y: int }\nfn f(p: Point) {\n  p.\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(2, 4))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "x: int"), "field `x: int`: {labels:?}");
    assert!(labels.iter().any(|l| l == "y: int"), "field `y: int`: {labels:?}");
}

#[test]
fn completion_member_on_struct_literal_let_and_chain() {
    // receiver `p` is an inferred `let` bound to a struct literal; chained field.
    let src = "struct Vec2 { x: float, y: float }\nstruct Player { pos: Vec2, name: string }\nfn main() {\n  let p = Player { pos: Vec2 { x: 0.0, y: 0.0 }, name: \"h\" }\n  p.pos.\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(4, 8))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "x: float"), "chained field `x: float`: {labels:?}");
}

#[test]
fn completion_member_filters_by_field_prefix() {
    let src = "struct Player { position_x: float, health: float }\nfn foo(player: Player) {\n  player.pos\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(2, 12))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "position_x: float"), "prefix `pos`: {labels:?}");
    assert!(!labels.iter().any(|l| l == "health: float"), "`health` excluded by prefix `pos`: {labels:?}");
}

#[test]
fn completion_member_on_for_loop_var() {
    // `item` is a for-loop variable; its struct is the element type of the
    // iterable `[Item]`. Resolved structurally (no inference).
    let src = "struct Item { name: string, count: int }\nfn main() {\n  let items: [Item] = vec_new()\n  for item in items {\n    item.\n  }\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(4, 9))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "name: string"), "loop-var field `name`: {labels:?}");
    assert!(labels.iter().any(|l| l == "count: int"), "loop-var field `count`: {labels:?}");
}

#[test]
fn completion_enum_qualified_lists_variants() {
    let src = "enum Direction { Up, Down, Left, Right }\nfn main() {\n  let d = Direction::\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(2, 21))
        .into_iter()
        .map(|i| i.label)
        .collect();
    for v in ["Up", "Down", "Left", "Right"] {
        assert!(labels.iter().any(|l| l == v), "enum variant {v}: {labels:?}");
    }
}

#[test]
fn completion_member_on_match_pattern_binding() {
    // `p` is bound by `Opt::Some(p)`; its struct is the variant's payload type.
    let src = "struct Point { x: int, y: int }\nenum Opt { None, Some(Point) }\nfn main() {\n  let o = Opt::Some(Point { x: 1, y: 2 })\n  match o {\n    Opt::Some(p) => p.\n    Opt::None => {}\n  }\n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(5, 22))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "x: int"), "pattern-bound field `x`: {labels:?}");
    assert!(labels.iter().any(|l| l == "y: int"), "pattern-bound field `y`: {labels:?}");
}

#[test]
fn completion_enum_record_variant_fields() {
    let src = "enum Effect { Bandage { duration: float, heal_amount: int }, Other(int) }\nfn main() {\n  let e = Effect::Bandage { \n}\n";
    let mut bone = fresh(src);
    let labels: Vec<String> = bone
        .completion(PATH, Position::new(2, 27))
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "duration: float"), "record field `duration`: {labels:?}");
    assert!(labels.iter().any(|l| l == "heal_amount: int"), "record field `heal_amount`: {labels:?}");
}

#[test]
fn unknown_file_degrades_to_empty() {
    let bone = BoneWorkspace::new();
    // Never updated -> no session for this path.
    assert!(bone.hover("/tmp/never_opened.rock", Position::new(0, 0)).is_none());
    assert!(bone.diagnostics_for("/tmp/never_opened.rock").is_empty());
}

#[test]
fn pure_fn_calling_stdlib_lerp_is_not_flagged_impure() {
    // Regression: the engine's `@pure fn lerp(Vec2,..)` delegates to the stdlib
    // `lerp(float,..)`. bone's `@pure` check is annotation-based (a @pure body
    // may only call @pure callees), so the stdlib lerp must itself be `@pure`
    // (it is -- pure arithmetic). Otherwise a @pure caller is wrongly flagged
    // "not provably pure" -- a divergence from rock, which infers the callee's
    // purity. (`stone/engine/math/types.rock` hit this in the wild.)
    let src = "@pure\nfn lerp2(a: float, b: float, t: float) -> float { return lerp(a, b, t) }\n";
    let bone = fresh(src);
    let diags = bone.diagnostics_for(PATH);
    assert!(
        !diags.iter().any(|d| d.message.contains("provably pure")),
        "stdlib lerp must be @pure so a @pure caller passes; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn definition_of_stdlib_type_resolves_to_a_real_file() {
    // Goto-definition on a stdlib type (`Entity`) must return a Location whose
    // URI is a real, openable absolute path -- NOT `file://unified.rock` (a
    // malformed URI where `unified.rock` is parsed as the host, leaving an empty
    // path). The bad URI plus a stdlib-relative line crashed nvim with
    // "Invalid cursor line: out of range" when it applied the line to the
    // (empty) buffer. Coroutine code triggers this constantly via `Entity<T>` /
    // `Coroutine<...>`.
    let src = "struct Mob { health: int }\nfn f(e: Entity<Mob>) {}\n";
    let bone = fresh(src);
    // `Entity` in `e: Entity<Mob>` on line 1, around char 8.
    let loc = bone
        .definition(PATH, Position::new(1, 9))
        .expect("goto-def on a stdlib type should resolve");
    let uri = loc.uri.as_str();
    assert!(
        uri.starts_with("file:///") && uri.ends_with("unified.rock"),
        "stdlib def URI must be an absolute file path, got {uri:?}"
    );
    // And the resolved file must actually contain `struct Entity` at the
    // reported line (the offsets are valid against the on-disk stdlib).
    let path = uri.strip_prefix("file://").unwrap();
    let text = std::fs::read_to_string(path).expect("stdlib file must be readable");
    let line = loc.range.start.line as usize;
    let target = text.lines().nth(line).unwrap_or("");
    assert!(
        target.contains("Entity"),
        "line {line} of the resolved stdlib should declare Entity, got {target:?}"
    );
}

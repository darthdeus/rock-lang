//! Semantic-token classification: each token's type and modifiers.

use crate::fixture::{PATH, Project, decode_tokens, workspace};
use bone_query::{BoneWorkspace, semantic_tokens_legend, semantic_tokens_modifiers_legend};

/// Absolute `(line, col, "type.modifier...")` tokens, modifiers in legend
/// bit order, like nvim's `@lsp.typemod` groups.
fn rendered(bone: &BoneWorkspace, path: &str) -> Vec<(u32, u32, String)> {
    let types = semantic_tokens_legend();
    let mods = semantic_tokens_modifiers_legend();
    let tokens = decode_tokens(&bone.semantic_tokens(path).data);
    tokens
        .into_iter()
        .map(|(line, col, t)| {
            let mut name = types[t.token_type as usize].as_str().to_string();
            for (i, m) in mods.iter().enumerate() {
                if t.token_modifiers_bitset & (1 << i) != 0 {
                    name = format!("{name}.{}", m.as_str());
                }
            }
            (line, col, name)
        })
        .collect()
}

#[track_caller]
fn assert_tokens(case: &str, toks: &[(u32, u32, String)], expected: &[(u32, u32, &str)]) {
    for (line, col, want) in expected {
        let got = toks
            .iter()
            .find(|(l, c, _)| l == line && c == col)
            .map(|(_, _, t)| t.as_str());
        assert_eq!(got, Some(*want), "{case}: token at {line}:{col}");
    }
}

#[test]
fn token_classification() {
    // (case, source, expected tokens at (line, start column))
    let cases: &[(&str, &str, &[(u32, u32, &str)])] = &[
        (
            "struct reference, field decl and literal field name",
            "struct FixVec2 { x: int, y: int }\nfn run() {\n  let v = FixVec2 { x: 1, y: 2 }\n  let n = v.x\n}\n",
            &[
                (2, 10, "struct"),
                (0, 17, "property"),
                (2, 20, "property"),
                (3, 12, "property"),
            ],
        ),
        (
            "parameter decl and use",
            "struct FixVec2 { x: int }\nfn f(a: FixVec2) -> int {\n  a.x\n}\n",
            &[(1, 5, "parameter"), (2, 2, "parameter")],
        ),
        // `nullable` marks `T?` bindings where flow has not narrowed them;
        // `refined` replaces it where it has.
        (
            "nullable param decl and unrefined use",
            "fn f(x: int?) -> int {\n  log(x)\n  return 0\n}\n",
            &[(0, 5, "parameter.nullable"), (1, 6, "parameter.nullable")],
        ),
        (
            "narrowed use is refined, not nullable",
            "fn f(x: int?) -> int {\n  if x != null {\n    return x\n  }\n  return 0\n}\n",
            &[(2, 11, "parameter.refined")],
        ),
        (
            "non-nullable binding",
            "fn f(x: int) -> int {\n  return x\n}\n",
            &[(0, 5, "parameter"), (1, 9, "parameter")],
        ),
        (
            "nullable let decl",
            "fn f(x: int?) -> int {\n  let y: int? = x\n  return 0\n}\n",
            &[(1, 6, "variable.nullable")],
        ),
        // The `.`-member heuristic must defer to known function names.
        (
            "UFCS method call",
            "struct Mob { hp: int }\nfn resolve_strike(m: Mob) {}\nfn run(m: Mob) {\n  m.resolve_strike()\n}\n",
            &[(3, 4, "function")],
        ),
        // `co` is contextual so coroutine values can be bound to it.
        (
            "contextual co",
            "co fn gen() -> Coroutine<int> {\n  yield 1\n}\nfn run() {\n  let co = gen()\n  let r = co.resume()\n}\n",
            &[(0, 0, "keyword"), (4, 6, "variable")],
        ),
        // The HIR SiblingRef must not override the member to `variable`.
        (
            "field-ref key path",
            "struct Mob { health: int, attack_co: int? }\nfn poll_slot(f: FieldRef<Mob, int?>) {}\nfn main() {\n  poll_slot(.attack_co)\n}\n",
            &[(3, 13, "property")],
        ),
        // Mutable globals carry `static`, consts `readonly`; locals neither.
        (
            "global and local",
            "global SPEED: int = 10\nfn f() -> int {\n  let local_v = SPEED\n  return local_v\n}\n",
            &[
                (0, 7, "variable.static"),
                (2, 16, "variable.static"),
                (2, 6, "variable"),
                (3, 9, "variable"),
            ],
        ),
        (
            "const decl and use",
            "const LIMIT: int = 10\nfn f() -> int {\n  return LIMIT\n}\n",
            &[(0, 6, "variable.readonly"), (2, 9, "variable.readonly")],
        ),
        (
            "nullable global",
            "global TARGET: int? = null\nfn f() {\n  log(TARGET)\n}\n",
            &[
                (0, 7, "variable.nullable.static"),
                (2, 6, "variable.nullable.static"),
            ],
        ),
        (
            "narrowed global",
            "global TARGET: int? = null\nfn f() -> int {\n  if TARGET != null {\n    return TARGET\n  }\n  return 0\n}\n",
            &[(3, 11, "variable.refined.static")],
        ),
        (
            "enum variant decl and qualified use",
            "enum Color { Red, Green }\nfn f() -> Color {\n  return Color::Red\n}\n",
            &[(0, 13, "enumMember"), (2, 9, "enum"), (2, 16, "enumMember")],
        ),
        (
            "enum variants in match patterns",
            "enum Color { Red, Green }\nfn f(c: Color) -> int {\n  return match c {\n    Color::Red => 1\n    Color::Green => 2\n  }\n}\n",
            &[(3, 11, "enumMember"), (4, 11, "enumMember")],
        ),
        // Literal segments, braces included, stay strings around the holes.
        (
            "interpolation holes",
            "fn f(x: int) {\n  let s = \"v={x} w={x}\"\n}\n",
            &[
                (1, 14, "parameter"),
                (1, 20, "parameter"),
                (1, 10, "string"),
                (1, 15, "string"),
                (1, 21, "string"),
            ],
        ),
        (
            "global in an interpolation hole",
            "global SPEED: int = 10\nfn f() {\n  let s = \"speed={SPEED}\"\n}\n",
            &[(2, 18, "variable.static")],
        ),
        (
            "fn generic decl and uses",
            "fn id<T>(x: T) -> T {\n  return x\n}\n",
            &[
                (0, 6, "typeParameter"),
                (0, 12, "typeParameter"),
                (0, 18, "typeParameter"),
            ],
        ),
        (
            "struct generic decl and field use",
            "struct Box<T> { v: T }\nfn f(b: Box<int>) {}\n",
            &[(0, 11, "typeParameter"), (0, 19, "typeParameter")],
        ),
        (
            "deco def name, user and builtin annotations",
            "deco init capture {\n}\n@capture\nfn f() {}\n@nomangle\nfn g() {}\n",
            &[
                (0, 10, "decorator"),
                (2, 1, "decorator"),
                (4, 1, "decorator"),
            ],
        ),
        (
            "trailing-lambda @ operand",
            "fn run(g: fn() -> int) -> int { g() }\nfn f(x: int) {\n  let s = run() @ x\n}\n",
            &[(2, 18, "parameter")],
        ),
        // Macros expand before lowering; names come from the parse-time capture.
        (
            "macro def name and call site",
            "macro twice(x: int) -> int {\n  x * 2\n}\nfn f() -> int {\n  return twice(3)\n}\n",
            &[(0, 6, "macro"), (4, 9, "macro")],
        ),
        // Expansion stamps the invocation span onto template nodes; the macro name must still win.
        (
            "macro call whose template starts with a global",
            "global SPEED: int = 10\nmacro get_speed() -> int {\n  SPEED\n}\nfn f() -> int {\n  return get_speed()\n}\n",
            &[(5, 9, "macro")],
        ),
        (
            "contextual static keyword",
            "fn counter() -> int {\n    static n = 0\n    return n\n}\n",
            &[(1, 4, "keyword")],
        ),
        // A same-named global fn must not claim a named-argument label.
        (
            "named arg labels",
            "fn width() -> int { return 5 }\nfn make(width: int, height: int) -> int { return width + height }\nfn r() -> int {\n    return make(\n        width: 3,\n        height: 4,\n    )\n}\n",
            &[(4, 8, "parameter"), (5, 8, "parameter")],
        ),
        (
            "rock directive comment",
            "fn run() {\n  // rock: allow(unused_var)\n  // just a note\n  let x = 1\n}\n",
            &[(1, 2, "macro"), (2, 2, "comment")],
        ),
    ];
    for (case, src, expected) in cases {
        assert_tokens(case, &rendered(&workspace(src), PATH), expected);
    }
}

#[test]
fn cross_file_struct_name_colors_as_struct() {
    let project = Project::new(&[
        (
            "stick.toml",
            "[package]\nname = \"xp\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
        ),
        ("main.rock", "fn main() -> int { 0 }\n"),
        ("a.rock", "pub mod a;\npub struct Mob { pub hp: int }\n"),
        (
            "b.rock",
            "pub mod b;\nuse super::a::Mob;\nfn make() -> Mob {\n  Mob { hp: 1 }\n}\n",
        ),
    ]);
    let (bone, _) = project.open("b.rock");
    let toks = rendered(&bone, &project.path("b.rock"));
    assert_tokens("cross-file struct", &toks, &[(2, 13, "struct")]);
}

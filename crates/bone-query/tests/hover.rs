//! LSP hover token resolution. Two concerns:
//!  - Braces: hovering a real `{` / `}` resolves to the enclosing Block node
//!    (the only lapper entries a Block emits -- see alloc_block_expr in
//!    bone-lower). A silent miss means the block was allocated through plain
//!    `alloc_expr` (no markers) instead of `alloc_block_expr`.
//!  - Tokens: keywords and punctuation resolve to the enclosing construct;
//!    whitespace resolves to nothing. See hover token resolution contract.

use crate::fixture::{PATH, assert_text, hover_markup, pos, workspace};
use bone_query::BoneWorkspace;
use lsp_types::Position;

fn last_char(src: &str, needle: &str) -> Position {
    pos(src, needle, needle.len() - 1)
}

fn hover_at(bone: &BoneWorkspace, pos: Position) -> String {
    hover_markup(bone, PATH, pos).unwrap_or_default()
}

#[test]
fn brace_hover_resolves_the_block_in_every_braced_form() {
    // Each block's tail is a MarkerS literal, so the BLOCK hovers "MarkerS"
    // while any enclosing node that might shadow the brace offset hovers
    // something else (or nothing). (name, source, text ending at the open
    // brace, text ending at the close brace).
    let pre = "struct MarkerS { f: int }\n";
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "fn body",
            "fn f() -> MarkerS {\n  MarkerS { f: 1 }\n}\n",
            "-> MarkerS {",
            "\n}",
        ),
        (
            "block expression",
            "fn f() {\n  let x = {\n    MarkerS { f: 1 }\n  }\n}\n",
            "x = {",
            "}\n  }",
        ),
        (
            "if branch",
            "fn f() {\n  let x = if true {\n    MarkerS { f: 1 }\n  } else {\n    MarkerS { f: 2 }\n  }\n}\n",
            "if true {",
            "f: 1 }\n  }",
        ),
        (
            "if-let then",
            "fn f(a: int?) {\n  if let v = a {\n    MarkerS { f: v }\n  }\n}\n",
            "a {",
            "}\n  }",
        ),
        (
            "if-let else",
            "fn f(a: int?) {\n  if let v = a {\n    MarkerS { f: v }\n  } else {\n    MarkerS { f: 0 }\n  }\n}\n",
            "else {",
            "f: 0 }\n  }",
        ),
        (
            "lifecycle hook body",
            "fn f() {\n  on_killed {\n    MarkerS { f: 1 }\n  }\n}\n",
            "on_killed {",
            "}\n  }",
        ),
        (
            "decorator body",
            "deco init d {\n  MarkerS { f: 1 }\n}\nfn f() {}\n",
            "deco init d {",
            "\n}",
        ),
    ];

    let mut fails = Vec::new();
    for (name, src, open, close) in cases {
        let full = format!("{pre}{src}");
        let bone = workspace(&full);
        let o = hover_at(&bone, last_char(&full, open));
        let c = hover_at(&bone, last_char(&full, close));
        if !o.contains("MarkerS") || !c.contains("MarkerS") || o != c {
            fails.push(format!("  [{name}] open={o:?} close={c:?}"));
        }
    }
    assert!(
        fails.is_empty(),
        "braces not resolving their block (allocated via alloc_expr instead of alloc_block_expr?):\n{}",
        fails.join("\n")
    );
}

const SRC: &str = "\
enum Dir { North, South }
struct FixVec2 { x: int, y: int }
fn pick(d: Dir) -> int {
    let v = FixVec2 { x: 3, y: 5 }
    let r = if d == Dir::North { 1 } else { 2 }
    match d {
        Dir::North => 1
        Dir::South => 2
    }
    return r
}
";

/// Keywords and punctuation hover their construct; whitespace hovers nothing.
#[test]
fn token_hover_resolves_the_enclosing_construct() {
    // The match is a statement, so its value type is `()`. The signature hover
    // ends at the body brace, so body keywords must not show it.
    // (line, token on that line, offset into it, must contain, must not contain)
    let cases: &[(usize, &str, usize, &str, &str)] = &[
        (5, "match", 1, "Type:", "fn pick("),
        (6, "=>", 0, "Type:", "fn pick("),
        (6, "::", 0, "enum Dir", "fn pick("),
        (3, ": 3", 0, "x: int (field of FixVec2)", "fn pick("),
        (2, "fn", 0, "fn pick(", "match"),
        (2, "(", 0, "fn pick(", "match"),
        (2, "->", 0, "fn pick(", "match"),
        (9, "return", 0, "Type: int", "fn pick("),
        (4, "if", 0, "Type: int", "fn pick("),
        (4, "else", 0, "Type: int", "fn pick("),
    ];
    let bone = workspace(SRC);
    for (line, token, into, wanted, unwanted) in cases {
        let text = SRC.lines().nth(*line).unwrap();
        let col = text.find(token).unwrap() + into;
        let p = Position::new(*line as u32, col as u32);
        let h = hover_markup(&bone, PATH, p).unwrap_or_else(|| panic!("`{token}`: no hover"));
        assert!(h.contains(wanted), "`{token}`: {h:?}");
        assert!(!h.contains(unwanted), "`{token}`: {h:?}");
    }
    // Whitespace, and a `let` keyword (which must not leak the signature).
    for p in [
        Position::new(5, 0),
        Position::new(6, 0),
        Position::new(3, 4),
    ] {
        assert_eq!(hover_markup(&bone, PATH, p), None, "at {p:?}");
    }
}

#[test]
fn hover_cases() {
    let overloads = "struct A {}\nstruct B {}\nstruct C {}\nfn foo(a: A) -> int { 1 }\nfn foo(b: B) -> int { 2 }\n";
    let resolved = format!("{overloads}fn main() {{\n  let x = A {{}}\n  let r = foo(x)\n}}\n");
    let unresolved = format!("{overloads}fn main() {{\n  let c = C {{}}\n  let r = foo(c)\n}}\n");
    // (case, source, needle, offset, text fragments; `!` forbids a fragment)
    let cases: &[(&str, &str, &str, usize, &[&str])] = &[
        (
            "resolved overloaded call lists every overload",
            &resolved,
            "= foo",
            4,
            &["fn foo(a: A) -> int", "fn foo(b: B) -> int"],
        ),
        // With no resolution, the name still shows what shapes exist.
        (
            "unresolvable overloaded call lists every overload",
            &unresolved,
            "= foo",
            4,
            &["fn foo(a: A) -> int", "fn foo(b: B) -> int"],
        ),
        // Inference types an assign target `()`; hover shows the binding.
        (
            "assign target",
            "struct Widget { id: int }\nfn f(acc: Widget, other: Widget) {\nacc = other\n}\n",
            "acc = other",
            1,
            &["Widget", "!()"],
        ),
        (
            "field ref names the field, not the FieldRef wrapper",
            "struct Mob { health: int, attack_co: int? }\nfn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n    kill_co(mob, .attack_co)\n}\n",
            ".attack_co",
            9,
            &["attack_co", "field of Mob", "!FieldRef"],
        ),
    ];
    for (case, src, needle, into, patterns) in cases {
        let h = hover_at(&workspace(src), pos(src, needle, *into));
        assert_text(case, &h, patterns);
    }
    let h = hover_at(&workspace(&resolved), pos(&resolved, "= foo", 4));
    assert!(
        h.find("fn foo(a: A)") < h.find("fn foo(b: B)"),
        "the resolved overload comes first: {h:?}"
    );
}

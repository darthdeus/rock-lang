// Brace-hover coverage: hovering a real `{` / `}` resolves to the enclosing
// Block node (the only lapper entries a Block emits -- see alloc_block_expr in
// bone-lower). Every braced-block form must produce the SAME hover at its
// opening and closing brace; a silent miss here means the block was allocated
// through plain `alloc_expr` (no markers) instead of `alloc_block_expr`.

use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position};

const PATH: &str = "/probe/main.rock";

fn pos_of(src: &str, needle: &str) -> Position {
    let idx = src.find(needle).expect("needle") + needle.len() - 1;
    let before = &src[..idx];
    let line = before.matches('\n').count() as u32;
    let col = (idx - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position::new(line, col)
}

fn hover_at(bone: &BoneWorkspace, pos: Position) -> String {
    match bone.hover(PATH, pos).map(|h| h.contents) {
        Some(HoverContents::Markup(m)) => m.value,
        _ => String::new(),
    }
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
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, full.to_string());
        let o = hover_at(&bone, pos_of(&full, open));
        let c = hover_at(&bone, pos_of(&full, close));
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

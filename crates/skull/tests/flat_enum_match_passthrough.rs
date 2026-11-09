use crate::common::{emit_module, function_body, mangled_name, require_luajit, run_luajit};

// A `match` over a flat enum whose catch-all arm returns the scrutinee
// (`other => other`) joins in the enum's leaves. The bind used to box the
// scrutinee into a `{__variant ...}` table and the join unpacked it again,
// one table per call on an interpreted path.
const SRC: &str = r#"
enum Src { Player, Named(string), Mob(int, int) }
nobox value Ev { src: Src, n: int }

fn field_join(ev: Ev, s: string) -> Ev {
    return Ev { ..ev, src: match ev.src { Src::Named(l) => Src::Named("{l}{s}"), other => other } }
}

fn let_join(ev: Ev, s: string) -> Ev {
    let src = match ev.src { Src::Named(l) => Src::Named("{l}{s}"), other => other }
    return Ev { ..ev, src }
}

fn param_join(src: Src, s: string) -> Src {
    return match src { Src::Named(l) => Src::Named("{l}{s}"), other => other }
}

fn render(src: Src) -> string {
    return match src {
        Src::Player => "player",
        Src::Named(l) => l,
        Src::Mob(a, b) => "mob {a} {b}",
    }
}

fn maybe_join(src: Src?, s: string) -> Src? {
    let inner = src ?? return null
    return if s == "" { null } else {
        match inner { Src::Named(l) => Src::Named("{l}{s}"), other => other }
    }
}

@nomangle fn probe() -> string {
    let a = field_join(Ev { src: Src::Mob(1, 2), n: 3 }, "x")
    let b = let_join(Ev { src: Src::Named("q"), n: 4 }, "x")
    let c = param_join(Src::Player, "x")
    let d = maybe_join(Src::Mob(5, 6), "x")
    let e = maybe_join(null, "x")
    return "{render(a.src)} {a.n}|{render(b.src)} {b.n}|{render(c)}|{render(d ?? Src::Player)}|{e == null}"
}
"#;

#[test]
fn a_flat_enum_match_returning_its_scrutinee_does_not_box() {
    let lua = emit_module(SRC);
    for name in ["field_join", "let_join", "param_join"] {
        let body = function_body(&lua, &mangled_name(&lua, name));
        assert!(
            !body.contains("{__variant"),
            "`{name}` boxes the flat enum at its match join:\n{body}"
        );
        // A box the scalarizer removed still leaves its `__ty` leaf and nil
        // guard behind: the catch-all bind must pass the cells through.
        assert!(
            !body.contains("\"Src\""),
            "`{name}` rebuilds the scrutinee for its catch-all bind:\n{body}"
        );
    }
}

#[test]
fn a_flat_enum_match_returning_its_scrutinee_keeps_every_variant() {
    require_luajit();
    let mut lua = emit_module(SRC);
    lua.push_str("\nprint(probe())\n");
    assert_eq!(run_luajit(&lua), "mob 1 2 3|qx 4|player|mob 5 6|true");
}

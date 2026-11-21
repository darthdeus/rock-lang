use crate::common::{emit_module, function_body, mangled_name, run_luajit};

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
